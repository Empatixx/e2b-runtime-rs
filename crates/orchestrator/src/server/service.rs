use std::sync::Arc;
use std::time::SystemTime;

use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::lifecycle::{Command, SandboxHandle};
use super::node::{Lifetime, Node, egress_rules};
use crate::proto::sandbox_service_server::SandboxService;
use crate::proto::{
    RunningSandbox, SandboxCheckpointRequest, SandboxCheckpointResponse, SandboxCreateRequest, SandboxCreateResponse,
    SandboxDeleteRequest, SandboxListResponse, SandboxPauseRequest, SandboxPauseResponse, SandboxUpdateRequest,
};

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
        tokio::spawn(self.node.clone().create(config, lifetime))
            .await
            .map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))??;
        Ok(Response::new(SandboxCreateResponse {
            client_id: self.node.node_id.clone(),
            ..SandboxCreateResponse::default()
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
        let id = request.into_inner().sandbox_id;
        self.running(&id, &format!("sandbox '{id}' not found"))?;
        let handle = self
            .node
            .registry
            .stop(&id)
            .ok_or_else(|| Status::internal(format!("failed to delete sandbox '{id}'")))?;
        let _ = handle.commands.send(Command::Delete).await;
        Ok(Response::new(()))
    }

    async fn pause(&self, request: Request<SandboxPauseRequest>) -> Result<Response<SandboxPauseResponse>, Status> {
        let request = request.into_inner();
        let id = request.sandbox_id;
        self.running(&id, "sandbox not found")?;
        let build_id = Uuid::parse_str(&request.build_id)
            .map_err(|e| Status::invalid_argument(format!("invalid build id '{}': {e}", request.build_id)))?;
        let handle = self
            .node
            .registry
            .stop(&id)
            .ok_or_else(|| Status::internal("failed to pause sandbox"))?;
        let snapshotted = tokio::spawn(pause(handle, build_id, request.template_id))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        snapshotted
            .map(|()| Response::new(SandboxPauseResponse::default()))
            .map_err(|e| {
                Status::internal(format!(
                    "error snapshotting sandbox '{id}': error snapshotting sandbox: {e}"
                ))
            })
    }

    async fn checkpoint(
        &self,
        _request: Request<SandboxCheckpointRequest>,
    ) -> Result<Response<SandboxCheckpointResponse>, Status> {
        Err(Status::unimplemented("checkpoint is not supported"))
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

async fn pause(handle: SandboxHandle, build_id: Uuid, template_id: String) -> Result<(), String> {
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
    snapshot
        .await
        .map_err(|_| "the sandbox stopped".to_owned())?
        .map(|_| ())
}

fn system_time(timestamp: prost_types::Timestamp) -> SystemTime {
    SystemTime::try_from(timestamp).unwrap_or(SystemTime::UNIX_EPOCH)
}
