use std::path::Path;
use std::sync::Arc;

use opentelemetry::KeyValue;
use serde_json::json;
use tokio::sync::oneshot;
use tonic::Status;
use uuid::Uuid;

use super::route::BalloonMode;
use super::{CheckpointRequest, object_metadata, trace};
use crate::events::{CHECKPOINTED, KILLED, SandboxEvent};
use crate::featureflags::{
    DEFER_MEMORY_EXPORT, FS_FREEZE_VIA_EXEC, GUEST_SYNC_TIMEOUT_MS, PEER_TO_PEER_ASYNC_CHECKPOINT,
    SYNC_WP_TRACKER_DIRTY,
};
use crate::proto::SchedulingMetadata;
use crate::sandbox::snapshot::filesystem::FilesystemSnapshot;
use crate::sandbox::snapshot::in_place::memory::{MemoryCheckpoint, MemoryCheckpointRequest};
use crate::sandbox::snapshot::in_place::{
    FilesystemCheckpoint, FilesystemCheckpointRequest, InPlaceFailure, LostSandbox, QuiesceFlags,
};
use crate::server::build_id;
use crate::server::lifecycle::{Command, SandboxHandle, SandboxInfo};
use crate::server::node::prefetch::pause_contexts;
use crate::server::node::{Node, event_sandbox, execution};
use crate::server::scheduling::from_headers;
use crate::storage::backend::ObjectMetadata;
use crate::telemetry::report::ReportStatus;
use crate::telemetry::server::sandbox::record_kill;
use crate::template::metadata::{TemplateMetadata, TemplateVersions};

const KILL_REASON_RESUME_FAILED: &str = "resume_failed";
const KILL_REASON_THAW_FAILED: &str = "thaw_failed";
const METADATA_FILE: &str = "metadata.json";

pub fn lost_sandbox_kill_reason(lost: LostSandbox) -> &'static str {
    match lost {
        LostSandbox::ThawFailed => KILL_REASON_THAW_FAILED,
        LostSandbox::ResumeFailed => KILL_REASON_RESUME_FAILED,
    }
}

impl Node {
    pub(super) async fn checkpoint_in_place(
        self: &Arc<Self>,
        handle: SandboxHandle,
        request: CheckpointRequest,
        balloon_mode: BalloonMode,
    ) -> Result<(SchedulingMetadata, bool), Status> {
        let parent = handle.info.lock().clone();
        let id = parent.config.sandbox_id.clone();
        let _guard = handle.begin_in_place_checkpoint().ok_or_else(|| {
            Status::failed_precondition(format!("a checkpoint is already in progress for sandbox '{id}'"))
        })?;
        if !request.filesystem_only {
            return self.checkpoint_memory(&handle, &parent, request, balloon_mode).await;
        }
        let attributes = [KeyValue::new("sandbox.id", id.clone())];
        let snapshot_error = |e: &str| Status::failed_precondition(format!("error snapshotting sandbox '{id}': {e}"));
        let build_id = build_id::parse(&request.build_id)
            .map_err(|e| snapshot_error(&format!("error snapshotting sandbox: failed to parse build id: {e}")))?;
        let dir = self.templates.instance_dir(build_id);
        let checkpoint = self
            .take_filesystem_checkpoint(&handle, &parent, &dir, build_id, balloon_mode)
            .await
            .map_err(|failure| self.checkpoint_failure(&parent, failure))
            .report_critical("error snapshotting sandbox for checkpoint", &attributes)?;
        let layer = self
            .cache_checkpoint(&parent, &checkpoint, &dir)
            .await
            .inspect_err(|_| discard(&dir))
            .map_err(|e| snapshot_error(&format!("error snapshotting sandbox: {e}")))
            .report_critical("error snapshotting sandbox for checkpoint", &attributes)?;
        let object_metadata = object_metadata(request.provenance, &parent);
        self.persist_filesystem_checkpoint(layer, object_metadata, &id, &attributes)
            .await?;
        self.events
            .publish(SandboxEvent::new(CHECKPOINTED, &event_sandbox(&parent)));
        Ok((from_headers(build_id, None, &checkpoint.build.rootfs, 0), false))
    }

    async fn checkpoint_memory(
        self: &Arc<Self>,
        handle: &SandboxHandle,
        parent: &SandboxInfo,
        request: CheckpointRequest,
        balloon_mode: BalloonMode,
    ) -> Result<(SchedulingMetadata, bool), Status> {
        let id = parent.config.sandbox_id.clone();
        let attributes = [KeyValue::new("sandbox.id", id.clone())];
        let snapshot_error = |e: &str| Status::failed_precondition(format!("error snapshotting sandbox '{id}': {e}"));
        let build_id = build_id::parse(&request.build_id)
            .map_err(|e| snapshot_error(&format!("error snapshotting sandbox: failed to parse build id: {e}")))?;
        let dir = self.templates.instance_dir(build_id);
        let contexts = pause_contexts(&parent.config);
        let memory_request = MemoryCheckpointRequest {
            dir: dir.clone(),
            build_id,
            team_id: parent.config.team_id.clone(),
            balloon_mode: balloon_mode.as_str().to_owned(),
            defer_export: self.feature_flags.get(&DEFER_MEMORY_EXPORT, &contexts),
            tracker_dirty: parent.use_sync_wp && self.feature_flags.get(&SYNC_WP_TRACKER_DIRTY, &contexts),
        };
        let checkpoint = self
            .take_memory_checkpoint(handle, memory_request)
            .await
            .map_err(|failure| self.checkpoint_failure(parent, failure))
            .report_critical("error snapshotting sandbox for checkpoint", &attributes)?;
        let deferred = checkpoint.deferred;
        let local = self
            .adopt_snapshot(checkpoint.snapshot, parent, build_id, dir.clone())
            .await
            .map_err(|e| snapshot_error(&e))
            .report_critical("error snapshotting sandbox for checkpoint", &attributes)?;
        self.embed_prefetch_from(
            trace(handle).await,
            &self.templates.get_for(build_id, parent.config.max_sandbox_length),
            &local,
        )
        .await;
        let object_metadata = object_metadata(request.provenance, parent);
        if let Err(status) = self.persist_checkpoint(&local, object_metadata, &id, &attributes).await {
            self.abandon_checkpoint(build_id);
            return Err(status);
        }
        self.events
            .publish(SandboxEvent::new(CHECKPOINTED, &event_sandbox(parent)));
        let build = &local.snapshot.build;
        Ok((
            from_headers(
                build.build_id,
                Some(&build.memfile),
                &build.rootfs,
                local.snapshot.memfile_bytes,
            ),
            deferred,
        ))
    }

    async fn take_memory_checkpoint(
        &self,
        handle: &SandboxHandle,
        request: MemoryCheckpointRequest,
    ) -> Result<MemoryCheckpoint, InPlaceFailure> {
        let (reply, checkpoint) = oneshot::channel();
        let stopped = || InPlaceFailure {
            message: "the sandbox stopped".to_owned(),
            lost: None,
        };
        handle
            .commands
            .send(Command::CheckpointMemoryInPlace { request, reply })
            .await
            .map_err(|_| stopped())?;
        checkpoint.await.map_err(|_| stopped())?
    }

    async fn take_filesystem_checkpoint(
        &self,
        handle: &SandboxHandle,
        parent: &SandboxInfo,
        dir: &Path,
        build_id: Uuid,
        balloon_mode: BalloonMode,
    ) -> Result<FilesystemCheckpoint, InPlaceFailure> {
        let contexts = pause_contexts(&parent.config);
        let request = FilesystemCheckpointRequest {
            dir: dir.to_path_buf(),
            build_id,
            flags: QuiesceFlags {
                sync_timeout_override_ms: self.feature_flags.get(&GUEST_SYNC_TIMEOUT_MS, &contexts),
                freeze_via_exec: self.feature_flags.get(&FS_FREEZE_VIA_EXEC, &contexts),
            },
            team_id: parent.config.team_id.clone(),
            balloon_mode: balloon_mode.as_str().to_owned(),
        };
        let (reply, checkpoint) = oneshot::channel();
        let stopped = || InPlaceFailure {
            message: "the sandbox stopped".to_owned(),
            lost: None,
        };
        handle
            .commands
            .send(Command::CheckpointInPlace { request, reply })
            .await
            .map_err(|_| stopped())?;
        checkpoint.await.map_err(|_| stopped())?
    }

    fn checkpoint_failure(&self, parent: &SandboxInfo, failure: InPlaceFailure) -> Status {
        let id = &parent.config.sandbox_id;
        let message = format!(
            "error snapshotting sandbox '{id}': error snapshotting sandbox: {}",
            failure.message
        );
        match failure.lost {
            Some(lost) => {
                self.publish_killed(parent, lost_sandbox_kill_reason(lost));
                Status::internal(message)
            }
            None => Status::failed_precondition(message),
        }
    }

    fn publish_killed(&self, parent: &SandboxInfo, kill_reason: &str) {
        record_kill(&parent.config.team_id, kill_reason);
        self.events.publish(
            SandboxEvent::new(KILLED, &event_sandbox(parent))
                .with_execution(&execution(parent))
                .with("kill_reason", json!(kill_reason)),
        );
    }

    async fn cache_checkpoint(
        &self,
        parent: &SandboxInfo,
        checkpoint: &FilesystemCheckpoint,
        dir: &Path,
    ) -> Result<FilesystemSnapshot, String> {
        let build = &checkpoint.build;
        let metadata = self.checkpoint_metadata(parent, checkpoint).await;
        let metadata_file = dir.join(METADATA_FILE);
        std::fs::write(&metadata_file, metadata.to_json())
            .map_err(|e| format!("write {}: {e}", metadata_file.display()))?;
        let layer = FilesystemSnapshot {
            build: build.clone(),
            rootfs_diff: (checkpoint.rootfs_bytes > 0).then(|| checkpoint.rootfs_diff.clone()),
            rootfs_bytes: checkpoint.rootfs_bytes,
            metadata_file,
        };
        self.cache_filesystem_layer(&layer, &metadata, dir.to_path_buf())
            .map_err(|e| format!("error adding snapshot to template cache: {e}"))?;
        self.peers.register(build.build_id).await;
        Ok(layer)
    }

    async fn checkpoint_metadata(&self, parent: &SandboxInfo, checkpoint: &FilesystemCheckpoint) -> TemplateMetadata {
        let metadata = match &parent.metadata {
            Some(metadata) => metadata.clone(),
            None => self.parent_metadata(&parent.config.build_id).await,
        }
        .same_version(TemplateVersions {
            build_id: checkpoint.build.build_id.to_string(),
            kernel_version: parent.config.kernel_version.clone(),
            firecracker_version: parent.config.firecracker_version.clone(),
        });
        let metadata = match checkpoint.vm_vcpus {
            0 => metadata,
            vcpus => metadata.with_vcpu_count(vcpus),
        };
        metadata.mark_filesystem_only(true, checkpoint.frozen)
    }

    pub(super) async fn persist_filesystem_checkpoint(
        &self,
        layer: FilesystemSnapshot,
        object_metadata: ObjectMetadata,
        id: &str,
        attributes: &[KeyValue],
    ) -> Result<(), Status> {
        let build_id = layer.build.build_id;
        let upload = self.spawn_filesystem_upload(layer, object_metadata);
        let peers = self.peers.clone();
        let finished = async move {
            let uploaded = upload.await;
            peers.finish(build_id, uploaded.is_ok()).await;
            uploaded
        };
        match self.feature_flags.get(&PEER_TO_PEER_ASYNC_CHECKPOINT, &[]) {
            true => {
                tokio::spawn(finished);
                Ok(())
            }
            false => finished
                .await
                .map_err(|e| {
                    Status::failed_precondition(format!("error uploading snapshot for checkpoint '{id}': {e}"))
                })
                .report_critical("error uploading snapshot for checkpoint", attributes),
        }
    }
}

fn discard(dir: &Path) {
    drop(std::fs::remove_dir_all(dir));
}
