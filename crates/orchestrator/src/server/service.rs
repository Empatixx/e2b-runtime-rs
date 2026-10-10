use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::KeyValue;
use tonic::{Request, Response, Status};
use tracing::{Instrument, Span};

use super::build_id;
use super::lifecycle::{Command, SandboxHandle};
use super::node::{CheckpointRequest, Lifetime, Node, egress_rules, event_sandbox, execution};
use super::pause_disk::PauseDiskReservation;
use super::registry::Lifecycle;
use super::scheduling::from_headers;
use super::vcpus::validate_vcpus;
use crate::envd::version::check_for_snapshot;
use crate::events::{KILLED, SandboxEvent, UPDATED};
use crate::go_time::rfc3339;
use crate::proto::sandbox_service_server::SandboxService;
use crate::proto::{
    RunningSandbox, SandboxCheckpointRequest, SandboxCheckpointResponse, SandboxCreateRequest, SandboxCreateResponse,
    SandboxDeleteRequest, SandboxListResponse, SandboxPauseRequest, SandboxPauseResponse, SandboxUpdateRequest,
};
use crate::sandbox::snapshot::Snapshot;
use crate::telemetry::report::{ReportStatus, report_critical_error, report_error};
use crate::telemetry::server::sandbox::{CreateOutcome, PauseOutcome, record_kill};
use crate::telemetry::server::spans;
use serde_json::json;

const FS_RECOVERY_NONE: &str = "none";
const STOPPED: &str = "error snapshotting sandbox: the sandbox stopped";

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
        let span = spans::sandbox_create(request.get_ref().sandbox.as_ref());
        let started = Instant::now();
        let resume = request
            .get_ref()
            .sandbox
            .as_ref()
            .is_some_and(|sandbox| sandbox.snapshot);
        let fs_boot_requested = request.get_ref().filesystem_boot.unwrap_or_default();
        let team_id = request
            .get_ref()
            .sandbox
            .as_ref()
            .map(|sandbox| sandbox.team_id.clone())
            .unwrap_or_default();
        let created = self.create_sandbox(request).instrument(span).await;
        CreateOutcome {
            resume,
            fs_only: created.as_ref().is_ok_and(|created| created.filesystem_only),
            fs_boot_requested,
            success: created.is_ok(),
            envd_upgraded: false,
            fs_recovery: FS_RECOVERY_NONE.to_owned(),
            team_id,
        }
        .record(started.elapsed());
        created.map(|created| created.response)
    }

    async fn update(&self, request: Request<SandboxUpdateRequest>) -> Result<Response<()>, Status> {
        let span = spans::sandbox_update(&request.get_ref().sandbox_id);
        self.update_sandbox(request).instrument(span).await
    }

    async fn list(&self, request: Request<()>) -> Result<Response<SandboxListResponse>, Status> {
        self.list_sandboxes(request).instrument(spans::sandbox_list()).await
    }

    async fn delete(&self, request: Request<SandboxDeleteRequest>) -> Result<Response<()>, Status> {
        let span = spans::sandbox_delete(&request.get_ref().sandbox_id);
        let kill_reason = request.get_ref().kill_reason.clone().unwrap_or_default();
        let team_id = self.team_of(&request.get_ref().sandbox_id);
        let deleted = self.delete_sandbox(request).instrument(span).await;
        if deleted.is_ok() {
            record_kill(&team_id, &kill_reason);
        }
        deleted
    }

    async fn pause(&self, request: Request<SandboxPauseRequest>) -> Result<Response<SandboxPauseResponse>, Status> {
        let span = spans::sandbox_pause(request.get_ref());
        let started = Instant::now();
        let fs_only = request.get_ref().filesystem_only;
        let team_id = self.team_of(&request.get_ref().sandbox_id);
        let paused = self.pause_sandbox(request).instrument(span).await;
        PauseOutcome {
            fs_only,
            success: paused.is_ok(),
            team_id,
        }
        .record(started.elapsed());
        paused
    }

    async fn checkpoint(
        &self,
        request: Request<SandboxCheckpointRequest>,
    ) -> Result<Response<SandboxCheckpointResponse>, Status> {
        let span = spans::sandbox_checkpoint(request.get_ref());
        self.checkpoint_sandbox(request).instrument(span).await
    }
}

impl SandboxServer {
    async fn create_sandbox(&self, request: Request<SandboxCreateRequest>) -> Result<CreatedResponse, Status> {
        let created = self.start_sandbox(request).await;
        report_create_failure(&created);
        if created.is_ok() {
            spans::record_balloon_mode();
        }
        created
    }

    async fn start_sandbox(&self, request: Request<SandboxCreateRequest>) -> Result<CreatedResponse, Status> {
        let request = request.into_inner();
        let filesystem_boot_requested = request.filesystem_boot.unwrap_or_default();
        let config = request
            .sandbox
            .ok_or_else(|| Status::invalid_argument("sandbox config is required"))?;
        validate_vcpus(config.vcpu, config.max_vcpus)?;
        let lifetime = Lifetime {
            start: request.start_time.map_or(SystemTime::UNIX_EPOCH, system_time),
            end: request.end_time.map_or(SystemTime::UNIX_EPOCH, system_time),
        };
        let resolved_firecracker_version = config.firecracker_version.clone();
        let created = tokio::spawn(self.node.clone().create(config, lifetime, filesystem_boot_requested))
            .await
            .map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))??;
        Ok(CreatedResponse {
            filesystem_only: created.filesystem_only,
            response: Response::new(SandboxCreateResponse {
                client_id: self.node.node_id.clone(),
                scheduling_metadata: Some(created.scheduling),
                filesystem_boot_applied: created.filesystem_booted,
                resolved_firecracker_version,
                network_placement: None,
            }),
        })
    }

    async fn update_sandbox(&self, request: Request<SandboxUpdateRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let handle = self.running(&request.sandbox_id, "sandbox not found", &[])?;
        spans::record_running(&handle.info.lock().config);
        let _serialised = handle.updates.lock().await;
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
                let failure = format!("failed to update sandbox network: {e}");
                report_critical_error("failed to update sandbox", Some(&failure), &[]);
                return Err(Status::internal(format!("failed to update sandbox: {failure}")));
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

    async fn list_sandboxes(&self, _request: Request<()>) -> Result<Response<SandboxListResponse>, Status> {
        let sandboxes = self
            .node
            .registry
            .running()
            .iter()
            .map(|handle| self.describe(handle))
            .collect();
        Ok(Response::new(SandboxListResponse { sandboxes }))
    }

    async fn delete_sandbox(&self, request: Request<SandboxDeleteRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let id = request.sandbox_id;
        let running = self.running(&id, &format!("sandbox '{id}' not found"), &sandbox_attribute(&id))?;
        spans::record_running(&running.info.lock().config);
        let lifecycle = running.lifecycle();
        let handle = self
            .node
            .mark_stopping(&lifecycle)
            .await
            .ok_or_else(|| Status::internal(format!("failed to delete sandbox '{id}'")))
            .report_critical("failed to mark sandbox as stopping", &sandbox_attribute(&id))?;
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

    async fn pause_sandbox(
        &self,
        request: Request<SandboxPauseRequest>,
    ) -> Result<Response<SandboxPauseResponse>, Status> {
        let request = request.into_inner();
        let id = request.sandbox_id;
        let running = self.running(&id, "sandbox not found", &sandbox_attribute(&id))?;
        spans::record_running(&running.info.lock().config);
        let lifecycle = running.lifecycle();
        let disk = self.admit_pause_disk(&running, &lifecycle, request.filesystem_only)?;
        let handle = self
            .node
            .mark_stopping(&lifecycle)
            .await
            .ok_or_else(|| Status::internal("failed to pause sandbox"))
            .report_critical("failed to mark sandbox as stopping", &sandbox_attribute(&id))?;
        let snapshotted = tokio::spawn(pause(handle, request.build_id, request.template_id, disk).in_current_span())
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
            .map_err(|e| Status::internal(format!("error snapshotting sandbox '{id}': {e}")))
            .report_critical("error snapshotting sandbox", &sandbox_attribute(&id))
    }

    async fn checkpoint_sandbox(
        &self,
        request: Request<SandboxCheckpointRequest>,
    ) -> Result<Response<SandboxCheckpointResponse>, Status> {
        let request = request.into_inner();
        let id = request.sandbox_id;
        let handle = self.running(&id, &format!("sandbox '{id}' not found"), &sandbox_attribute(&id))?;
        spans::record_running(&handle.info.lock().config);
        let envd_version = handle.info.lock().config.envd_version.clone();
        check_for_snapshot(&envd_version).map_err(Status::failed_precondition)?;
        let checkpoint = CheckpointRequest {
            build_id: request.build_id,
            provenance: request.metadata,
            filesystem_only: request.filesystem_only,
        };
        tokio::spawn(self.node.clone().checkpoint(handle, checkpoint).in_current_span())
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
    fn admit_pause_disk(
        &self,
        running: &SandboxHandle,
        lifecycle: &Lifecycle,
        filesystem_only: bool,
    ) -> Result<Option<Arc<PauseDiskReservation>>, Status> {
        let info = running.info.lock();
        self.node.admit_pause_disk(
            &info.config.sandbox_id,
            &info.config.team_id,
            info.config.ram_mb,
            &self.node.rootfs_cache_of(lifecycle),
            filesystem_only,
        )
    }

    fn team_of(&self, sandbox_id: &str) -> String {
        self.node
            .registry
            .get(sandbox_id)
            .map(|handle| handle.info.lock().config.team_id.clone())
            .unwrap_or_default()
    }

    fn running(&self, id: &str, missing: &str, attributes: &[KeyValue]) -> Result<SandboxHandle, Status> {
        self.node.registry.get(id).ok_or_else(|| {
            report_critical_error("sandbox not found", None, attributes);
            Status::not_found(missing.to_owned())
        })
    }

    fn describe(&self, handle: &SandboxHandle) -> RunningSandbox {
        let info = handle.info.lock().clone();
        RunningSandbox {
            config: Some(info.config.clone()),
            client_id: self.node.node_id.clone(),
            start_time: Some(info.start.into()),
            end_time: Some(info.end.into()),
            sandbox_id: info.config.sandbox_id,
            team_id: info.config.team_id,
            execution_id: info.config.execution_id,
            vcpu: info.config.vcpu,
            ram_mb: info.config.ram_mb,
        }
    }
}

async fn pause(
    handle: SandboxHandle,
    build_id: String,
    template_id: String,
    disk: Option<Arc<PauseDiskReservation>>,
) -> Result<Snapshot, String> {
    let build_id = match build_id::parse(&build_id) {
        Ok(build_id) => build_id,
        Err(e) => {
            let _ = handle.commands.send(Command::Delete).await;
            return Err(format!("error snapshotting sandbox: failed to parse build id: {e}"));
        }
    };
    let (reply, snapshot) = tokio::sync::oneshot::channel();
    handle
        .commands
        .send(Command::Pause {
            build_id,
            template_id,
            span: Span::current(),
            reply,
            disk,
        })
        .await
        .map_err(|_| STOPPED.to_owned())?;
    snapshot.await.map_err(|_| STOPPED.to_owned())?
}

fn sandbox_attribute(id: &str) -> [KeyValue; 1] {
    [KeyValue::new("sandbox.id", id.to_owned())]
}

struct CreatedResponse {
    response: Response<SandboxCreateResponse>,
    filesystem_only: bool,
}

fn report_create_failure(created: &Result<CreatedResponse, Status>) {
    let Err(status) = created else { return };
    match (status.code(), status.message().starts_with("sandbox files for ")) {
        (tonic::Code::FailedPrecondition, true) => {
            report_error("sandbox files not found", Some("object does not exist"), &[])
        }
        (tonic::Code::AlreadyExists, _) => report_critical_error(
            "refusing to create sandbox: its ID is already taken on this node",
            Some(status.message()),
            &[],
        ),
        _ => report_critical_error("failed to create sandbox", Some(status.message()), &[]),
    }
}

fn system_time(timestamp: prost_types::Timestamp) -> SystemTime {
    let nanos = i128::from(timestamp.seconds) * 1_000_000_000 + i128::from(timestamp.nanos);
    let offset = Duration::new(
        (nanos.unsigned_abs() / 1_000_000_000) as u64,
        (nanos.unsigned_abs() % 1_000_000_000) as u32,
    );
    match nanos >= 0 {
        true => SystemTime::UNIX_EPOCH.checked_add(offset),
        false => SystemTime::UNIX_EPOCH.checked_sub(offset),
    }
    .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp(seconds: i64, nanos: i32) -> prost_types::Timestamp {
        prost_types::Timestamp { seconds, nanos }
    }

    #[test]
    fn timestamps_convert_like_go_time_unix() {
        assert_eq!(
            system_time(timestamp(10, 500_000_000)),
            SystemTime::UNIX_EPOCH + Duration::from_millis(10_500)
        );
        assert_eq!(
            system_time(timestamp(10, -500_000_000)),
            SystemTime::UNIX_EPOCH + Duration::from_millis(9_500)
        );
        assert_eq!(
            system_time(timestamp(10, 1_500_000_000)),
            SystemTime::UNIX_EPOCH + Duration::from_millis(11_500)
        );
        assert_eq!(
            system_time(timestamp(-5, 0)),
            SystemTime::UNIX_EPOCH - Duration::from_secs(5)
        );
    }
}
