use std::sync::Arc;
use std::time::SystemTime;

use tonic::{Request, Response, Status};

use super::build_id;
use super::lifecycle::{Command, SandboxHandle};
use super::node::{CheckpointRequest, Lifetime, Node, egress_rules, event_sandbox, execution};
use super::scheduling::from_headers;
use crate::envd::version::check_for_snapshot;
use crate::events::{KILLED, SandboxEvent, UPDATED};
use crate::go_time::rfc3339;
use crate::proto::sandbox_service_server::SandboxService;
use crate::proto::{
    RunningSandbox, SandboxCheckpointRequest, SandboxCheckpointResponse, SandboxCreateRequest, SandboxCreateResponse,
    SandboxDeleteRequest, SandboxListResponse, SandboxPauseRequest, SandboxPauseResponse, SandboxUpdateRequest,
};
use crate::sandbox::snapshot::Snapshot;
use serde_json::json;

pub struct SandboxServer {
    node: Arc<Node>,
}

impl SandboxServer {
    pub fn new(node: Arc<Node>) -> Self {
        Self { node }
    }
}

#[tonic::async_trait]
impl SandboxService for SandboxServer {
    async fn create(&self, request: Request<SandboxCreateRequest>) -> Result<Response<SandboxCreateResponse>, Status> {
        let request = request.into_inner();
        let config = request
            .sandbox
            .ok_or_else(|| Status::invalid_argument("sandbox config is required"))?;
        let lifetime = Lifetime {
            start: request.start_time.map_or(SystemTime::UNIX_EPOCH, system_time),
            end: request.end_time.map_or(SystemTime::UNIX_EPOCH, system_time),
        };
        let resolved_firecracker_version = config.firecracker_version.clone();
        let scheduling_metadata = tokio::spawn(self.node.clone().create(config, lifetime))
            .await
            .map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))??;
        Ok(Response::new(SandboxCreateResponse {
            client_id: self.node.node_id.clone(),
            scheduling_metadata: Some(scheduling_metadata),
            filesystem_boot_applied: false,
            resolved_firecracker_version,
        }))
    }

    async fn update(&self, request: Request<SandboxUpdateRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let handle = self.running(&request.sandbox_id, "sandbox not found")?;
        let rules = request
            .egress
            .as_ref()
            .map(|egress| egress_rules(Some(egress)).map(Option::unwrap_or_default))
            .transpose()?;
        let previous_end = handle.info.lock().end;
        let request_had_egress = request.egress.is_some();
        if let Some(end) = request.end_time {
            handle.info.lock().end = system_time(end);
        }
        if let (Some(rules), Some(egress)) = (rules, request.egress) {
            let slot = handle.info.lock().slot;
            if let Err(e) = self.node.network.configure(slot, &rules).await {
                handle.info.lock().end = previous_end;
                return Err(Status::internal(format!(
                    "failed to update sandbox: failed to update sandbox network: {e}"
                )));
            }
            handle
                .info
                .lock()
                .config
                .network
                .get_or_insert_with(Default::default)
                .egress = Some(egress);
        }
        if request.end_time.is_some() {
            let _ = handle.commands.send(Command::Extend).await;
        }
        let info = handle.info.lock().clone();
        let updated = request
            .end_time
            .map(|_| ("set_timeout", json!(rfc3339(info.end))))
            .into_iter()
            .chain(
                info.config
                    .network
                    .as_ref()
                    .and_then(|network| network.egress.as_ref())
                    .filter(|_| request_had_egress)
                    .map(|egress| {
                        (
                            "network_egress",
                            json!({
                                "allowed_cidrs": egress.allowed_cidrs,
                                "denied_cidrs": egress.denied_cidrs,
                                "allowed_domains": egress.allowed_domains,
                            }),
                        )
                    }),
            )
            .fold(
                SandboxEvent::new(UPDATED, &event_sandbox(&info)),
                |event, (key, value)| event.with(key, value),
            );
        if request.end_time.is_some() || request_had_egress {
            self.node.events.publish(updated);
        }
        Ok(Response::new(()))
    }

    async fn list(&self, _request: Request<()>) -> Result<Response<SandboxListResponse>, Status> {
        let sandboxes = self
            .node
            .registry
            .running()
            .iter()
            .map(|handle| self.describe(handle))
            .collect();
        Ok(Response::new(SandboxListResponse { sandboxes }))
    }

    async fn delete(&self, request: Request<SandboxDeleteRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let id = request.sandbox_id;
        let lifecycle = self.running(&id, &format!("sandbox '{id}' not found"))?.lifecycle();
        let handle = self
            .node
            .mark_stopping(&lifecycle)
            .await
            .ok_or_else(|| Status::internal(format!("failed to delete sandbox '{id}'")))?;
        let info = handle.info.lock().clone();
        let kill_reason = request.kill_reason.filter(|reason| !reason.is_empty());
        self.node.events.publish(
            SandboxEvent::new(KILLED, &event_sandbox(&info))
                .with_execution(&execution(&info))
                .with("kill_reason", json!(kill_reason.as_deref().unwrap_or("unknown"))),
        );
        let _ = handle.commands.send(Command::Delete).await;
        Ok(Response::new(()))
    }

    async fn pause(&self, request: Request<SandboxPauseRequest>) -> Result<Response<SandboxPauseResponse>, Status> {
        let request = request.into_inner();
        let id = request.sandbox_id;
        let lifecycle = self.running(&id, "sandbox not found")?.lifecycle();
        let handle = self
            .node
            .mark_stopping(&lifecycle)
            .await
            .ok_or_else(|| Status::internal("failed to pause sandbox"))?;
        let snapshotted = tokio::spawn(pause(handle, request.build_id, request.template_id))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        snapshotted
            .map(|snapshot| {
                Response::new(SandboxPauseResponse {
                    scheduling_metadata: Some(from_headers(
                        snapshot.build.build_id,
                        Some(&snapshot.build.memfile),
                        &snapshot.build.rootfs,
                        snapshot.memfile_bytes,
                    )),
                })
            })
            .map_err(|e| {
                Status::internal(format!(
                    "error snapshotting sandbox '{id}': error snapshotting sandbox: {e}"
                ))
            })
    }

    async fn checkpoint(
        &self,
        request: Request<SandboxCheckpointRequest>,
    ) -> Result<Response<SandboxCheckpointResponse>, Status> {
        let request = request.into_inner();
        let id = request.sandbox_id;
        let handle = self.running(&id, &format!("sandbox '{id}' not found"))?;
        let envd_version = handle.info.lock().config.envd_version.clone();
        check_for_snapshot(&envd_version).map_err(Status::failed_precondition)?;
        let checkpoint = CheckpointRequest {
            build_id: request.build_id,
            provenance: request.metadata,
            filesystem_only: request.filesystem_only,
        };
        tokio::spawn(self.node.clone().checkpoint(handle, checkpoint))
            .await
            .map_err(|e| Status::internal(format!("failed to checkpoint sandbox '{id}': {e}")))?
            .map(|scheduling_metadata| {
                Response::new(SandboxCheckpointResponse {
                    scheduling_metadata: Some(scheduling_metadata),
                })
            })
    }
}

impl SandboxServer {
    fn running(&self, id: &str, missing: &str) -> Result<SandboxHandle, Status> {
        self.node
            .registry
            .get(id)
            .ok_or_else(|| Status::not_found(missing.to_owned()))
    }

    fn describe(&self, handle: &SandboxHandle) -> RunningSandbox {
        let info = handle.info.lock().clone();
        RunningSandbox {
            client_id: self.node.node_id.clone(),
            start_time: Some(info.start.into()),
            end_time: Some(info.end.into()),
            sandbox_id: info.config.sandbox_id,
            team_id: info.config.team_id,
            execution_id: info.config.execution_id,
            vcpu: info.config.vcpu,
            ram_mb: info.config.ram_mb,
            ..RunningSandbox::default()
        }
    }
}

async fn pause(handle: SandboxHandle, build_id: String, template_id: String) -> Result<Snapshot, String> {
    let build_id = match build_id::parse(&build_id) {
        Ok(build_id) => build_id,
        Err(e) => {
            let _ = handle.commands.send(Command::Delete).await;
            return Err(format!("failed to parse build id: {e}"));
        }
    };
    let (reply, snapshot) = tokio::sync::oneshot::channel();
    handle
        .commands
        .send(Command::Pause {
            build_id,
            template_id,
            reply,
        })
        .await
        .map_err(|_| "the sandbox stopped".to_owned())?;
    snapshot.await.map_err(|_| "the sandbox stopped".to_owned())?
}

fn system_time(timestamp: prost_types::Timestamp) -> SystemTime {
    SystemTime::try_from(timestamp).unwrap_or(SystemTime::UNIX_EPOCH)
}
