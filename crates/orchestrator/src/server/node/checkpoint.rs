mod route;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use opentelemetry::KeyValue;
use tokio::sync::oneshot;
use tonic::Status;
use uuid::Uuid;

use self::route::{BalloonMode, balloon_route, early_route};
use super::prefetch::pause_contexts as flag_contexts;
use super::{Lifetime, Node, StartRequest, event_sandbox, routed_sandbox};
use crate::events::{CHECKPOINTED, SandboxEvent};
use crate::featureflags::{
    DEFER_MEMORY_EXPORT, IN_PLACE_CHECKPOINT, IN_PLACE_CHECKPOINT_REPORTING, PEER_TO_PEER_ASYNC_CHECKPOINT,
};
use crate::firecracker::version::supports_in_place_checkpoint;
use crate::memory::trace::Trace;
use crate::proto::SchedulingMetadata;
use crate::sandbox::snapshot::LocalSnapshot;
use crate::server::build_id;
use crate::server::lifecycle::{Command, SandboxHandle, SandboxInfo};
use crate::server::registry::{Lifecycle, StopReserved};
use crate::server::retry::ATTEMPT_TIMEOUT;
use crate::server::scheduling::from_headers;
use crate::server::user_error::filesystem_only_checkpoint_disabled;
use crate::storage::backend::ObjectMetadata;
use crate::telemetry::report::{ReportStatus, report_critical_error};
use crate::telemetry::server::sandbox::CheckpointOutcome;
use crate::telemetry::server::spans;
use crate::template::metadata::PREFETCH_ORIGIN_CHECKPOINT;

const STOPPED: &str = "error snapshotting sandbox: the sandbox stopped";
const BUILD_ORIGIN: &str = "snapshot_template";
const RESUMED_WITH_SYNC_WP: bool = false;

pub struct CheckpointRequest {
    pub build_id: String,
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
        let fs_only = request.filesystem_only;
        if fs_only {
            return Err(filesystem_only_checkpoint_disabled(&id));
        }
        let started = Instant::now();
        let team_id = handle.info.lock().config.team_id.clone();
        let (route, balloon_mode) = self.checkpoint_route(&handle.info.lock());
        spans::record_checkpoint_route(route, balloon_mode.as_str());
        let checkpointed = self.checkpoint_routed(handle, request, lifecycle).await;
        CheckpointOutcome {
            in_place: false,
            fs_only,
            route: route.to_owned(),
            balloon_mode: balloon_mode.as_str().to_owned(),
            deferred: false,
            success: checkpointed.is_ok(),
            team_id,
        }
        .record(started.elapsed());
        checkpointed
    }

    fn checkpoint_route(&self, info: &SandboxInfo) -> (&'static str, BalloonMode) {
        let flags = &self.feature_flags;
        let contexts = flag_contexts(&info.config);
        let firecracker_version = info.config.firecracker_version.as_str();
        let early = early_route(RESUMED_WITH_SYNC_WP, flags.get(&IN_PLACE_CHECKPOINT, &contexts), || {
            supports_in_place_checkpoint(firecracker_version)
        });
        let stamped = BalloonMode::stamped(info.metadata.as_ref().and_then(|metadata| metadata.balloon));
        match early {
            Some(route) => (route, stamped),
            None => (
                balloon_route(
                    BalloonMode::Unknown,
                    || flags.get(&DEFER_MEMORY_EXPORT, &contexts),
                    || flags.get(&IN_PLACE_CHECKPOINT_REPORTING, &contexts),
                ),
                BalloonMode::Unknown,
            ),
        }
    }

    async fn checkpoint_routed(
        self: &Arc<Self>,
        handle: SandboxHandle,
        request: CheckpointRequest,
        lifecycle: Lifecycle,
    ) -> Result<SchedulingMetadata, Status> {
        let id = lifecycle.sandbox_id.clone();
        let stopped = self
            .registry
            .stop_reserved(&lifecycle)
            .map_err(|stopped| stop_reserved_status(stopped, &id))
            .inspect_err(|status| report_marking_failure(status, &id))?;
        let routed = routed_sandbox(&stopped.info.lock());
        self.routing.on_stopping(&routed).await;
        let checkpointed = self.resume_fresh(handle, request).await;
        self.registry.release(&id);
        checkpointed
    }

    async fn resume_fresh(
        self: &Arc<Self>,
        handle: SandboxHandle,
        request: CheckpointRequest,
    ) -> Result<SchedulingMetadata, Status> {
        let parent = handle.info.lock().clone();
        let id = parent.config.sandbox_id.clone();
        let snapshot_error = |e: String| Status::internal(format!("error snapshotting sandbox '{id}': {e}"));
        let build_id = match build_id::parse(&request.build_id) {
            Ok(build_id) => build_id,
            Err(e) => {
                let _ = handle.commands.send(Command::Delete).await;
                return Err(snapshot_error(format!(
                    "error snapshotting sandbox: failed to parse build id: {e}"
                )));
            }
        };
        let attributes = [KeyValue::new("sandbox.id", id.clone())];
        let local = snapshot(&handle, build_id)
            .await
            .map_err(snapshot_error)
            .report_critical("error snapshotting sandbox for checkpoint", &attributes)?;
        let resumed = self
            .resume_checkpoint(&parent, build_id)
            .await
            .inspect_err(|_| self.abandon_checkpoint(build_id))
            .report_critical("error resuming sandbox after checkpoint", &attributes)?;
        self.embed_prefetch(&resumed, &local).await;
        let object_metadata = object_metadata(request.provenance, &parent);
        if let Err(status) = self.persist_checkpoint(&local, object_metadata, &id, &attributes).await {
            self.kill(&resumed).await;
            return Err(status);
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
                oom_watermark: parent.oom_watermark.clone(),
                throwaway: false,
            })
            .await
            .inspect_err(|_| drop(std::fs::remove_dir_all(self.work_dir_of(&lifecycle))))
            .map_err(|failure| {
                Status::internal(format!(
                    "error resuming sandbox after checkpoint: {}",
                    failure.message()
                ))
            })?;
        self.register(&lifecycle, &started)
            .await
            .map_err(|e| Status::internal(format!("error registering resumed sandbox after checkpoint: {e}")))?;
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
            .map(|metadata| metadata.with_prefetch(Some(prefetch), PREFETCH_ORIGIN_CHECKPOINT));
        let written = updated.map_err(|e| e.to_string()).and_then(|metadata| {
            std::fs::write(super::metadata_path(local), metadata.to_json())
                .map(|()| template.update_metadata(metadata))
                .map_err(|e| e.to_string())
        });
        if let Err(e) = written {
            tracing::warn!(build = %local.snapshot.build.build_id, error = %e, "failed to update local metadata with prefetch");
        }
    }

    async fn persist_checkpoint(
        &self,
        local: &LocalSnapshot,
        object_metadata: ObjectMetadata,
        id: &str,
        attributes: &[KeyValue],
    ) -> Result<(), Status> {
        match self.feature_flags.get(&PEER_TO_PEER_ASYNC_CHECKPOINT, &[]) {
            true => {
                self.detach_checkpoint_upload(local, object_metadata, id, attributes)
                    .await
            }
            false => self
                .upload_now(local.clone(), object_metadata)
                .await
                .map_err(|e| Status::internal(format!("error uploading snapshot for checkpoint '{id}': {e}")))
                .report_critical("error uploading snapshot for checkpoint", attributes),
        }
    }

    async fn detach_checkpoint_upload(
        &self,
        local: &LocalSnapshot,
        object_metadata: ObjectMetadata,
        id: &str,
        attributes: &[KeyValue],
    ) -> Result<(), Status> {
        let build_id = local.snapshot.build.build_id;
        let sealed = tokio::time::timeout(ATTEMPT_TIMEOUT, local.memfile_ready.clone())
            .await
            .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()));
        match sealed {
            Ok(()) => {
                self.upload(local.clone(), object_metadata, None);
                Ok(())
            }
            Err(e) => {
                self.abandon_checkpoint(build_id);
                Err(Status::internal(format!(
                    "deferred memory export for checkpoint '{id}' failed: {e}"
                )))
                .report_critical("deferred memory seal failed before checkpoint upload", attributes)
            }
        }
    }

    fn abandon_checkpoint(&self, build_id: Uuid) {
        self.store.unpin(build_id);
        let peers = self.peers.clone();
        tokio::spawn(async move { peers.finish(build_id, false).await });
    }

    async fn kill(&self, resumed: &SandboxHandle) {
        self.mark_stopping(&resumed.lifecycle()).await;
        let _ = resumed.commands.send(Command::Delete).await;
    }
}

async fn snapshot(handle: &SandboxHandle, build_id: Uuid) -> Result<LocalSnapshot, String> {
    let (reply, snapshot) = oneshot::channel();
    handle
        .commands
        .send(Command::Checkpoint { build_id, reply })
        .await
        .map_err(|_| STOPPED.to_owned())?;
    snapshot.await.map_err(|_| STOPPED.to_owned())?
}

async fn trace(handle: &SandboxHandle) -> Option<Trace> {
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

fn report_marking_failure(status: &Status, sandbox_id: &str) {
    if status.code() != tonic::Code::FailedPrecondition {
        report_critical_error(
            "failed to mark sandbox as stopping",
            Some(status.message()),
            &[KeyValue::new("sandbox.id", sandbox_id.to_owned())],
        );
    }
}

fn stop_reserved_status(stopped: StopReserved, id: &str) -> Status {
    match stopped {
        StopReserved::InProgress => {
            Status::failed_precondition(format!("an operation is already in progress for sandbox '{id}'"))
        }
        StopReserved::NotLive => Status::internal(format!("failed to checkpoint sandbox '{id}'")),
    }
}
