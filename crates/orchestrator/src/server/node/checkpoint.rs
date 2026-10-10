mod in_place;
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
use super::{Lifetime, Node, StartFailure, StartRequest, event_sandbox, routed_sandbox};
use crate::events::{CHECKPOINTED, SandboxEvent};
use crate::featureflags::{
    DEFER_MEMORY_EXPORT, FILESYSTEM_ONLY_CHECKPOINT, IN_PLACE_CHECKPOINT, IN_PLACE_CHECKPOINT_REPORTING,
    PEER_TO_PEER_ASYNC_CHECKPOINT,
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
        let (route, balloon_mode) = self.route_of(&handle.info.lock(), &id, fs_only)?;
        let started = Instant::now();
        let team_id = handle.info.lock().config.team_id.clone();
        spans::record_checkpoint_route(route, balloon_mode.as_str());
        let in_place = route == route::IN_PLACE;
        let checkpointed = match in_place {
            true => self.checkpoint_in_place(handle, request, balloon_mode).await,
            false => self.checkpoint_routed(handle, request, lifecycle).await,
        };
        CheckpointOutcome {
            in_place,
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

    fn route_of(&self, info: &SandboxInfo, id: &str, fs_only: bool) -> Result<(&'static str, BalloonMode), Status> {
        match fs_only {
            true => self
                .feature_flags
                .get(&FILESYSTEM_ONLY_CHECKPOINT, &flag_contexts(&info.config))
                .then(|| (route::IN_PLACE, stamped_balloon(info)))
                .ok_or_else(|| filesystem_only_checkpoint_disabled(id)),
            false => Ok(self.checkpoint_route(info)),
        }
    }

    fn checkpoint_route(&self, info: &SandboxInfo) -> (&'static str, BalloonMode) {
        let flags = &self.feature_flags;
        let contexts = flag_contexts(&info.config);
        let firecracker_version = info.config.firecracker_version.as_str();
        let early = early_route(RESUMED_WITH_SYNC_WP, flags.get(&IN_PLACE_CHECKPOINT, &contexts), || {
            supports_in_place_checkpoint(firecracker_version)
        });
        let stamped = stamped_balloon(info);
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
                upgrade_envd: true,
            })
            .await
            .inspect_err(|_| drop(std::fs::remove_dir_all(self.work_dir_of(&lifecycle))))
            .map_err(|failure| match failure {
                StartFailure::Unrecoverable(message) => Status::unknown(message),
                failure => Status::internal(format!(
                    "error resuming sandbox after checkpoint: {}",
                    failure.message()
                )),
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

fn stamped_balloon(info: &SandboxInfo) -> BalloonMode {
    BalloonMode::stamped(info.metadata.as_ref().and_then(|metadata| metadata.balloon))
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

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::SystemTime;

    use tonic::Code;

    use super::*;
    use crate::featureflags::FeatureFlags;
    use crate::network::slot::Slot;
    use crate::proto::SandboxConfig;
    use crate::server::config::NodeConfig;
    use crate::server::lifecycle::detached_handle;
    use crate::server::user_error::user_error_of;
    use crate::storage::backend::Storage;
    use crate::storage::build::Build;
    use crate::storage::header::{Header, Metadata};
    use crate::template::Template;
    use crate::template::metadata::{Balloon, TemplateMetadata};

    struct Fixture {
        node: Arc<Node>,
        _dir: tempfile::TempDir,
    }

    fn node(flags: &[(&str, bool)]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).display().to_string();
        let vars = [
            ("STORAGE_PROVIDER", "Local".to_owned()),
            ("LOCAL_TEMPLATE_STORAGE_BASE_PATH", path("store")),
            ("E2B_CACHE_DIR", path("cache")),
            ("E2B_WORK_DIR", path("sandboxes")),
            ("E2B_FIRECRACKER", "/unused".to_owned()),
            ("E2B_KERNEL", "/unused".to_owned()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
        let builder = flags.iter().fold(FeatureFlags::builder(), |builder, (key, value)| {
            builder.override_flag(key, *value)
        });
        let config = NodeConfig::from_vars(&vars)
            .unwrap()
            .with_feature_flags(builder.offline());
        Fixture {
            node: Arc::new(Node::new(&config).unwrap()),
            _dir: dir,
        }
    }

    fn info(balloon: Option<Balloon>, firecracker_version: &str) -> SandboxInfo {
        let dir = tempfile::tempdir().unwrap();
        let build_id = Uuid::new_v4();
        let rootfs = Header::new(Metadata::template(build_id, 4096, 4096), Vec::new()).unwrap();
        let metadata = TemplateMetadata {
            balloon,
            ..TemplateMetadata::default()
        };
        SandboxInfo {
            config: SandboxConfig {
                sandbox_id: "ckpt".to_owned(),
                team_id: "team-1".to_owned(),
                firecracker_version: firecracker_version.to_owned(),
                envd_version: "9.9.9".to_owned(),
                ..SandboxConfig::default()
            },
            start: SystemTime::now(),
            end: SystemTime::now(),
            metadata: Some(metadata.clone()),
            lifecycle: Lifecycle {
                sandbox_id: "ckpt".to_owned(),
                lifecycle_id: "lifecycle-1".to_owned(),
                execution_id: "execution-1".to_owned(),
            },
            host_ip: Ipv4Addr::LOCALHOST,
            envd_upgraded: false,
            slot: Slot::new(41).unwrap(),
            scheduling: SchedulingMetadata::default(),
            template: Arc::new(Template::filesystem_layer(
                Storage::local(dir.path()).unwrap(),
                Build::filesystem_only(build_id, rootfs),
                metadata,
                dir.path().to_path_buf(),
            )),
            started_at: SystemTime::now(),
            oom_watermark: Default::default(),
        }
    }

    #[tokio::test]
    async fn a_memory_checkpoint_never_goes_in_place_without_sync_wp_and_carries_the_stamp_as_its_label() {
        let fixture = node(&[("in-place-checkpoint", true), ("filesystem-only-checkpoint", true)]);
        let reporting = Balloon {
            reporting: true,
            hinting: false,
        };

        let (route, mode) = fixture
            .node
            .route_of(&info(Some(reporting), "v1.14-0.2.1"), "ckpt", false)
            .unwrap();

        assert_eq!((route, mode.as_str()), (route::SYNC_WP_OFF, "reporting"));
    }

    #[tokio::test]
    async fn a_filesystem_only_checkpoint_goes_in_place_on_its_own_flag() {
        let fixture = node(&[("in-place-checkpoint", false), ("filesystem-only-checkpoint", true)]);

        let (route, mode) = fixture.node.route_of(&info(None, "unparsable"), "ckpt", true).unwrap();

        assert_eq!((route, mode.as_str()), (route::IN_PLACE, "unknown"));
    }

    #[tokio::test]
    async fn a_filesystem_only_checkpoint_is_refused_while_its_flag_is_off() {
        let fixture = node(&[("in-place-checkpoint", true), ("filesystem-only-checkpoint", false)]);

        let refused = fixture
            .node
            .route_of(&info(None, "v1.14-0.2.1"), "ckpt", true)
            .unwrap_err();

        assert_eq!(refused.code(), Code::FailedPrecondition);
        assert_eq!(
            refused.message(),
            "filesystem-only checkpoint of sandbox 'ckpt' is disabled"
        );
        assert!(user_error_of(&refused).is_some());
    }

    #[tokio::test]
    async fn a_second_in_place_checkpoint_is_refused_while_one_is_in_flight() {
        let fixture = node(&[("filesystem-only-checkpoint", true)]);
        let (handle, _commands) = detached_handle(info(None, "v1.14-0.2.1"));
        let _held = handle.begin_in_place_checkpoint().expect("the first checkpoint starts");

        let refused = fixture
            .node
            .clone()
            .checkpoint(
                handle,
                CheckpointRequest {
                    build_id: Uuid::new_v4().to_string(),
                    provenance: HashMap::new(),
                    filesystem_only: true,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(refused.code(), Code::FailedPrecondition);
        assert!(
            refused.message().contains("already in progress"),
            "{}",
            refused.message()
        );
    }

    #[tokio::test]
    async fn the_in_place_guard_is_released_when_the_checkpoint_ends() {
        let (handle, _commands) = detached_handle(info(None, "v1.14-0.2.1"));

        let first = handle.begin_in_place_checkpoint();
        assert!(handle.begin_in_place_checkpoint().is_none());
        drop(first);

        assert!(handle.begin_in_place_checkpoint().is_some());
    }

    #[tokio::test]
    async fn an_in_place_checkpoint_of_a_stopped_sandbox_reports_that_it_stopped() {
        let fixture = node(&[("filesystem-only-checkpoint", true)]);
        let (handle, commands) = detached_handle(info(None, "v1.14-0.2.1"));
        drop(commands);

        let failed = fixture
            .node
            .clone()
            .checkpoint(
                handle,
                CheckpointRequest {
                    build_id: Uuid::new_v4().to_string(),
                    provenance: HashMap::new(),
                    filesystem_only: true,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(failed.code(), Code::FailedPrecondition);
        assert!(
            failed.message().ends_with("the sandbox stopped"),
            "{}",
            failed.message()
        );
    }

    #[tokio::test]
    async fn a_bad_build_id_is_refused_before_the_sandbox_is_touched() {
        let fixture = node(&[("filesystem-only-checkpoint", true)]);
        let (handle, mut commands) = detached_handle(info(None, "v1.14-0.2.1"));

        let refused = fixture
            .node
            .clone()
            .checkpoint(
                handle,
                CheckpointRequest {
                    build_id: "not-a-uuid".to_owned(),
                    provenance: HashMap::new(),
                    filesystem_only: true,
                },
            )
            .await
            .unwrap_err();

        assert_eq!(refused.code(), Code::FailedPrecondition);
        assert!(
            refused
                .message()
                .starts_with("error snapshotting sandbox 'ckpt': error snapshotting sandbox: failed to parse build id")
        );
        assert!(commands.try_recv().is_err());
    }

    fn vanished_layer() -> crate::sandbox::snapshot::filesystem::FilesystemSnapshot {
        let build_id = Uuid::new_v4();
        let rootfs = Header::new(Metadata::template(build_id, 4096, 4096), Vec::new()).unwrap();
        crate::sandbox::snapshot::filesystem::FilesystemSnapshot {
            build: Build::filesystem_only(build_id, rootfs),
            rootfs_diff: Some("/definitely/not/here/rootfs.diff".into()),
            rootfs_bytes: 4096,
            metadata_file: "/definitely/not/here/metadata.json".into(),
        }
    }

    #[tokio::test]
    async fn a_failed_checkpoint_upload_leaves_the_sandbox_alive_for_the_api() {
        let fixture = node(&[]);

        let failed = fixture
            .node
            .persist_filesystem_checkpoint(vanished_layer(), ObjectMetadata::default(), "ckpt", &[])
            .await
            .unwrap_err();

        assert_eq!(failed.code(), Code::FailedPrecondition);
        assert_eq!(
            failed.message(),
            "error uploading snapshot for checkpoint 'ckpt': snapshot files are gone"
        );
    }

    #[tokio::test]
    async fn an_async_checkpoint_upload_answers_before_it_lands() {
        let fixture = node(&[("peer-to-peer-async-checkpoint", true)]);

        let answered = fixture
            .node
            .persist_filesystem_checkpoint(vanished_layer(), ObjectMetadata::default(), "ckpt", &[])
            .await;

        assert!(answered.is_ok());
    }
}
