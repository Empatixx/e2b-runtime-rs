use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::oneshot;
use tonic::Status;
use uuid::Uuid;

use super::{Lifetime, Node, StartRequest, event_sandbox, routed_sandbox};
use crate::events::{CHECKPOINTED, SandboxEvent};
use crate::proto::SchedulingMetadata;
use crate::sandbox::snapshot::LocalSnapshot;
use crate::server::lifecycle::{Command, SandboxHandle, SandboxInfo};
use crate::server::registry::{Lifecycle, StopReserved};
use crate::server::scheduling::from_headers;
use crate::server::user_error::filesystem_only_checkpoint_disabled;
use crate::storage::backend::ObjectMetadata;

const BUILD_ORIGIN: &str = "snapshot_template";

pub struct CheckpointRequest {
    pub build_id: Uuid,
    pub provenance: HashMap<String, String>,
    pub filesystem_only: bool,
}

impl Node {
    pub async fn checkpoint(
        self: Arc<Self>,
        handle: SandboxHandle,
        request: CheckpointRequest,
    ) -> Result<SchedulingMetadata, Status> {
        let _work = self.status.track_work();
        let _permit = self.admission.acquire().await?;
        let lifecycle = handle.lifecycle();
        let id = lifecycle.sandbox_id.clone();
        if request.filesystem_only {
            return Err(filesystem_only_checkpoint_disabled(&id));
        }
        self.registry
            .stop_reserved(&lifecycle)
            .map_err(|stopped| stop_reserved_status(stopped, &id))?;
        self.resume_fresh(handle, request)
            .await
            .inspect_err(|_| self.registry.abandon(&id))
    }

    async fn resume_fresh(
        self: &Arc<Self>,
        handle: SandboxHandle,
        request: CheckpointRequest,
    ) -> Result<SchedulingMetadata, Status> {
        let parent = handle.info.lock().clone();
        let id = parent.config.sandbox_id.clone();
        let local = snapshot(&handle, request.build_id).await.map_err(|e| {
            Status::internal(format!(
                "error snapshotting sandbox '{id}': error snapshotting sandbox: {e}"
            ))
        })?;
        let resumed = self
            .resume_checkpoint(&parent, request.build_id)
            .await
            .inspect_err(|_| self.store.unpin(request.build_id))?;
        self.embed_prefetch(&resumed, &local).await;
        let object_metadata = object_metadata(request.provenance, &parent);
        if let Err(e) = self.upload_now(local.clone(), object_metadata).await {
            self.kill(&resumed).await;
            return Err(Status::internal(format!(
                "error uploading snapshot for checkpoint '{id}': {e}"
            )));
        }
        let info = resumed.info.lock().clone();
        self.events
            .publish(SandboxEvent::new(CHECKPOINTED, &event_sandbox(&info)));
        let build = &local.snapshot.build;
        Ok(from_headers(
            build.build_id,
            Some(&build.memfile),
            &build.rootfs,
            local.snapshot.memfile_bytes,
        ))
    }

    async fn resume_checkpoint(
        self: &Arc<Self>,
        parent: &SandboxInfo,
        build_id: Uuid,
    ) -> Result<SandboxHandle, Status> {
        let lifecycle = Lifecycle {
            sandbox_id: parent.lifecycle.sandbox_id.clone(),
            lifecycle_id: Uuid::new_v4().to_string(),
            execution_id: parent.lifecycle.execution_id.clone(),
        };
        let started = self
            .start(StartRequest {
                template: self.templates.get_for(build_id, parent.config.max_sandbox_length),
                config: parent.config.clone(),
                lifetime: Lifetime {
                    start: parent.start,
                    end: parent.end,
                },
                lifecycle: lifecycle.clone(),
                started_at: parent.started_at,
            })
            .await
            .inspect_err(|_| drop(std::fs::remove_dir_all(self.work_dir_of(&lifecycle))))
            .map_err(|status| {
                Status::internal(format!("error resuming sandbox after checkpoint: {}", status.message()))
            })?;
        let info = started.info.lock().clone();
        self.registry.activate(lifecycle, started.clone());
        self.routing.on_insert(&routed_sandbox(&info)).await;
        Ok(started)
    }

    async fn embed_prefetch(&self, resumed: &SandboxHandle, local: &LocalSnapshot) {
        let Some(prefetch) = trace(resumed).await.map(|trace| trace.to_mapping()) else {
            return;
        };
        if prefetch.indices.is_empty() {
            return;
        }
        let template = resumed.info.lock().template.clone();
        let updated = template
            .metadata()
            .await
            .map(|metadata| metadata.with_prefetch(Some(prefetch)));
        let written = updated.map_err(|e| e.to_string()).and_then(|metadata| {
            std::fs::write(super::metadata_path(local), metadata.to_json())
                .map(|()| template.update_metadata(metadata))
                .map_err(|e| e.to_string())
        });
        if let Err(e) = written {
            tracing::warn!(build = %local.snapshot.build.build_id, error = %e, "failed to update local metadata with prefetch");
        }
    }

    async fn kill(&self, resumed: &SandboxHandle) {
        self.registry.stop(&resumed.lifecycle());
        let _ = resumed.commands.send(Command::Delete).await;
    }
}

async fn snapshot(handle: &SandboxHandle, build_id: Uuid) -> Result<LocalSnapshot, String> {
    let (reply, snapshot) = oneshot::channel();
    handle
        .commands
        .send(Command::Checkpoint { build_id, reply })
        .await
        .map_err(|_| "the sandbox stopped".to_owned())?;
    snapshot.await.map_err(|_| "the sandbox stopped".to_owned())?
}

async fn trace(handle: &SandboxHandle) -> Option<crate::memory::trace::Trace> {
    let (reply, trace) = oneshot::channel();
    handle.commands.send(Command::Trace { reply }).await.ok()?;
    trace.await.ok().flatten()
}

fn object_metadata(provenance: HashMap<String, String>, parent: &SandboxInfo) -> ObjectMetadata {
    ObjectMetadata {
        entries: provenance.into_iter().collect(),
    }
    .with("team_id", &parent.config.team_id)
    .with("build_origin", BUILD_ORIGIN)
}

fn stop_reserved_status(stopped: StopReserved, id: &str) -> Status {
    match stopped {
        StopReserved::InProgress => {
            Status::failed_precondition(format!("an operation is already in progress for sandbox '{id}'"))
        }
        StopReserved::NotLive => Status::internal(format!("failed to checkpoint sandbox '{id}'")),
    }
}
