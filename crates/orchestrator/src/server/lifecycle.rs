mod health;
mod hinting;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use self::health::{HealthCheck, HealthLoop};
use self::hinting::HintLoop;
use std::time::{Duration, SystemTime};

use futures::FutureExt;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{Instrument, Span};
use uuid::Uuid;

use super::host_stats::HostStatsCollector;
use super::node::prefetch::PausedBuild;
use super::node::reclaim::{PausePreparation, reclaim_contexts};
use super::node::{NetworkAssignment, Node, event_sandbox, execution, stats_identity};
use super::oom_monitor::{OomMonitor, SharedWatermark};
use super::pause_disk::PauseDiskReservation;
use super::registry::Lifecycle;
use super::status::Work;
use crate::events::{PAUSED, SandboxEvent};
use crate::featureflags::DEFER_ROOTFS_EXPORT;
use crate::firecracker::exit::{CrashCause, ExitInfo};
use crate::firecracker::metrics::BalloonTotals;
use crate::memory::balloon_mode::BalloonMode;
use crate::memory::trace::Trace;
use crate::network::pool::Release;
use crate::network::slot::Slot;
use crate::proto::{SandboxConfig, SchedulingMetadata};
use crate::sandbox::hinting::USE_CASE_PAUSE;
use crate::sandbox::hinting::periodic::CheckpointMarks;
use crate::sandbox::snapshot::in_place::QuiesceFlags;
use crate::sandbox::snapshot::in_place::memory::{MemoryCheckpoint, MemoryCheckpointRequest};
use crate::sandbox::snapshot::in_place::{FilesystemCheckpoint, FilesystemCheckpointRequest, InPlaceFailure};
use crate::sandbox::snapshot::{LocalSnapshot, Snapshot};
use crate::sandbox::{Death, Sandbox};
use crate::sandbox_logs::{SandboxMeta, external};
use crate::storage::backend::ObjectMetadata;
use crate::telemetry::server::sandbox::{record_crash, record_execution};
use crate::telemetry::server::spans;
use crate::template::Template;
use crate::template::lineage::{Origin, SnapshotLineage};
use crate::template::metadata::TemplateMetadata;

const COMMAND_QUEUE: usize = 8;

#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub config: SandboxConfig,
    pub start: SystemTime,
    pub end: SystemTime,
    pub metadata: Option<TemplateMetadata>,
    pub lifecycle: Lifecycle,
    pub host_ip: Ipv4Addr,
    pub slot: Slot,
    pub scheduling: SchedulingMetadata,
    pub template: Arc<Template>,
    pub started_at: SystemTime,
    pub oom_watermark: SharedWatermark,
    pub envd_upgraded: bool,
    pub use_sync_wp: bool,
}

pub enum Command {
    Delete,
    Pause {
        build_id: Uuid,
        template_id: String,
        span: Span,
        reply: oneshot::Sender<Result<Snapshot, String>>,
        disk: Option<Arc<PauseDiskReservation>>,
        filesystem_only: Option<QuiesceFlags>,
    },
    Checkpoint {
        build_id: Uuid,
        reply: oneshot::Sender<Result<LocalSnapshot, String>>,
    },
    Trace {
        reply: oneshot::Sender<Option<Trace>>,
    },
    CheckpointInPlace {
        request: FilesystemCheckpointRequest,
        reply: oneshot::Sender<Result<FilesystemCheckpoint, InPlaceFailure>>,
    },
    CheckpointMemoryInPlace {
        request: MemoryCheckpointRequest,
        reply: oneshot::Sender<Result<MemoryCheckpoint, InPlaceFailure>>,
    },
    ResolveBalloonMode {
        reply: oneshot::Sender<BalloonMode>,
    },
    Extend,
}

#[derive(Clone)]
pub struct SandboxHandle {
    pub info: Arc<Mutex<SandboxInfo>>,
    pub commands: mpsc::Sender<Command>,
    pub updates: Arc<tokio::sync::Mutex<()>>,
    cleanup: Arc<AtomicBool>,
    checkpoints: Arc<CheckpointMarks>,
    health: Option<HealthCheck>,
    pause_balloon: Arc<PauseBalloon>,
}

#[derive(Debug, Default)]
pub struct PauseBalloon {
    requested: AtomicBool,
    totals: Mutex<Option<BalloonTotals>>,
}

pub struct InPlaceCheckpointGuard {
    checkpoints: Arc<CheckpointMarks>,
}

impl Drop for InPlaceCheckpointGuard {
    fn drop(&mut self) {
        self.checkpoints.end();
    }
}

impl SandboxHandle {
    pub fn begin_in_place_checkpoint(&self) -> Option<InPlaceCheckpointGuard> {
        self.checkpoints.begin().then(|| InPlaceCheckpointGuard {
            checkpoints: self.checkpoints.clone(),
        })
    }

    pub fn lifecycle(&self) -> Lifecycle {
        self.info.lock().lifecycle.clone()
    }

    pub fn record_balloon_after_pause(&self) {
        self.pause_balloon.requested.store(true, Ordering::SeqCst);
    }

    pub fn balloon_after_pause(&self) -> Option<BalloonTotals> {
        *self.pause_balloon.totals.lock()
    }

    pub async fn control_healthcheck(&self) {
        if let Some(health) = &self.health {
            health.check(true).await;
        }
    }

    pub fn cleanup_started(&self) -> bool {
        self.cleanup.load(Ordering::SeqCst)
    }
}

enum Ending {
    Stop {
        reason: String,
    },
    Crashed {
        death: Death,
    },
    Pause {
        build_id: Uuid,
        template_id: String,
        span: Span,
        reply: oneshot::Sender<Result<Snapshot, String>>,
        disk: Option<Arc<PauseDiskReservation>>,
        filesystem_only: Option<QuiesceFlags>,
    },
    Checkpoint {
        build_id: Uuid,
        reply: oneshot::Sender<Result<LocalSnapshot, String>>,
    },
}

const STOP_REASON_KILLED: &str = "killed";
const DELETED: &str = "deleted";
const STOP_REASON_PAUSED: &str = "paused";
const STOP_REASON_CHECKPOINTING: &str = "checkpointing";
const STOP_REASON_CRASHED: &str = "crashed";

impl Ending {
    fn stop_reason(&self) -> &'static str {
        match self {
            Self::Stop { .. } => STOP_REASON_KILLED,
            Self::Crashed { .. } => STOP_REASON_CRASHED,
            Self::Pause { .. } => STOP_REASON_PAUSED,
            Self::Checkpoint { .. } => STOP_REASON_CHECKPOINTING,
        }
    }

    fn record_metrics(&self, sandbox_id: &str, team_id: &str, ran_for: Duration) {
        if let Self::Crashed { death } = self {
            record_crash(team_id, death.cause.as_str());
            log_crash(sandbox_id, death);
        }
        record_execution(team_id, ran_for, self.stop_reason());
    }
}

fn log_crash(sandbox_id: &str, death: &Death) {
    let exit_fields = death.exit.as_ref().map(ExitInfo::log_fields).unwrap_or_default();
    let crash_cause = death.cause.as_str();
    match death.cause == CrashCause::CleanExit {
        true => {
            tracing::warn!(sandbox = %sandbox_id, %crash_cause, %exit_fields, "sandbox stopped without a stop reason")
        }
        false => {
            tracing::error!(sandbox = %sandbox_id, %crash_cause, %exit_fields, reason = %death.reason, "sandbox crashed")
        }
    }
}

struct Running {
    sandbox: Sandbox,
    slot: Slot,
    _network: NetworkAssignment,
    info: Arc<Mutex<SandboxInfo>>,
    cleanup: Arc<AtomicBool>,
    commands: mpsc::Receiver<Command>,
    node: Arc<Node>,
    host_stats: Option<HostStatsCollector>,
    _oom: Option<OomMonitor>,
    throwaway: bool,
    hint_loop: Option<HintLoop>,
    health_loop: Option<HealthLoop>,
    health: Option<HealthCheck>,
    pause_balloon: Arc<PauseBalloon>,
    checkpoints: Arc<CheckpointMarks>,
    _work: Work,
    _execution: Span,
}

#[cfg(test)]
pub fn detached_handle(info: SandboxInfo) -> (SandboxHandle, mpsc::Receiver<Command>) {
    let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
    let handle = SandboxHandle {
        info: Arc::new(Mutex::new(info)),
        commands,
        updates: Arc::default(),
        cleanup: Arc::default(),
        checkpoints: Arc::default(),
        health: None,
        pause_balloon: Arc::default(),
    };
    (handle, receiver)
}

pub fn spawn(
    sandbox: Sandbox,
    slot: Slot,
    network: NetworkAssignment,
    info: SandboxInfo,
    node: Arc<Node>,
    throwaway: bool,
) -> SandboxHandle {
    let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
    let health = (!throwaway).then(|| {
        HealthCheck::new(
            sandbox.envd_address(),
            SandboxMeta::of(&info.config),
            info.config.envd_version.clone(),
        )
    });
    let handle = SandboxHandle {
        info: Arc::new(Mutex::new(info)),
        commands,
        updates: Arc::default(),
        cleanup: Arc::default(),
        checkpoints: Arc::default(),
        health: health.clone(),
        pause_balloon: Arc::default(),
    };
    let host_stats = node
        .host_stats
        .get()
        .filter(|_| !throwaway)
        .zip(sandbox.cgroup_stats())
        .map(|(sink, reader)| HostStatsCollector::start(reader, stats_identity(&handle.info.lock()), sink.clone()));
    let (access_token, oom_watermark) = {
        let info = handle.info.lock();
        (info.config.envd_access_token.clone(), info.oom_watermark.clone())
    };
    let oom = (!throwaway).then(|| {
        OomMonitor::start(
            sandbox.envd_address(),
            access_token,
            SandboxMeta::of(&handle.info.lock().config),
            oom_watermark,
        )
    });
    let execution = {
        let info = handle.info.lock();
        let init_origin = info
            .metadata
            .as_ref()
            .map_or("none", TemplateMetadata::prefetch_init_origin);
        spans::execution(&info.config, sandbox.prefetch_summary().as_ref(), init_origin)
    };
    let hint_loop = (!throwaway)
        .then(|| HintLoop::start(&sandbox, &node, &handle.info.lock(), handle.checkpoints.clone()))
        .flatten();
    let running = Running {
        hint_loop,
        health_loop: health.clone().map(HealthLoop::start),
        health,
        pause_balloon: handle.pause_balloon.clone(),
        checkpoints: handle.checkpoints.clone(),
        _execution: execution,
        host_stats,
        _oom: oom,
        throwaway,
        sandbox,
        slot,
        _network: network,
        info: handle.info.clone(),
        cleanup: handle.cleanup.clone(),
        commands: receiver,
        _work: node.status.track_work(),
        node,
    };
    tokio::spawn(running.run());
    handle
}

impl Running {
    async fn run(self) {
        match self.throwaway {
            true => self.run_to_the_end().await,
            false => {
                let span = tracing::info_span!(parent: None, "stop sandbox-lifecycle");
                self.run_to_the_end().instrument(span).await;
            }
        }
    }

    async fn run_to_the_end(mut self) {
        let ending = self.wait_for_ending().await;
        let lifecycle = self.info.lock().lifecycle.clone();
        if !self.throwaway {
            let team_id = self.info.lock().config.team_id.clone();
            ending.record_metrics(self.sandbox.id(), &team_id, self.ran_for());
        }
        self.tear_down(ending, lifecycle).await;
    }

    async fn tear_down(mut self, ending: Ending, lifecycle: Lifecycle) {
        let id = self.sandbox.id().to_owned();
        if matches!(ending, Ending::Stop { .. } | Ending::Crashed { .. }) {
            self.sandbox.hinting().stopping.store(true, Ordering::SeqCst);
        }
        self.stop_checks().await;
        let deleted = matches!(&ending, Ending::Stop { reason } if reason == DELETED);
        self.cleanup.store(true, Ordering::SeqCst);
        self.node.mark_stopping(&lifecycle).await;
        match ending {
            Ending::Pause {
                build_id,
                template_id,
                span,
                reply,
                disk,
                filesystem_only,
            } => match filesystem_only {
                Some(flags) => {
                    self.pause_filesystem(flags, build_id, template_id, span, reply, disk)
                        .await
                }
                None => self.pause(build_id, template_id, span, reply, disk).await,
            },
            Ending::Checkpoint { build_id, reply } => self.checkpoint(build_id, reply).await,
            Ending::Stop { reason } => tracing::info!(sandbox = %id, %reason, "stopping sandbox"),
            Ending::Crashed { death } => tracing::info!(sandbox = %id, reason = %death.reason, "stopping sandbox"),
        }
        if let Some(collector) = self.host_stats.take() {
            collector.stop().await;
        }
        let stopped = self.sandbox.stop().await;
        let _ = std::fs::remove_dir_all(self.node.work_dir_of(&lifecycle));
        let release = network_release(self.node.clone(), lifecycle.lifecycle_id.clone());
        match stopped {
            Ok(()) => self.node.network.return_slot_with(self.slot, release),
            Err(e) => {
                tracing::error!(sandbox = %id, error = %e, "sandbox did not stop cleanly");
                self.node.network.discard_slot_with(self.slot, release);
            }
        }
        self.node
            .proxy
            .get()
            .iter()
            .for_each(|proxy| proxy.remove_from_pool(&lifecycle.lifecycle_id));
        self.node.registry.finish(&lifecycle);
        if deleted {
            external(&SandboxMeta::of(&self.info.lock().config)).info("Sandbox stopped", Vec::new());
        }
    }

    async fn pause_filesystem(
        &self,
        flags: QuiesceFlags,
        build_id: Uuid,
        template_id: String,
        span: Span,
        reply: oneshot::Sender<Result<Snapshot, String>>,
        disk: Option<Arc<PauseDiskReservation>>,
    ) {
        let parent = self.info.lock().clone();
        self.reclaim(&parent, true).await;
        let defer_rootfs = self.defers_rootfs(&parent);
        let paused = self
            .node
            .pause_filesystem_locally(&self.sandbox, &parent, build_id, flags, defer_rootfs)
            .await;
        if let Ok((layer, _)) = &paused {
            self.node.events.publish(
                SandboxEvent::new(PAUSED, &event_sandbox(&parent))
                    .with_execution(&execution(&parent))
                    .with_pause_mode(true),
            );
            let object_metadata = ObjectMetadata::default()
                .with("template_id", template_id)
                .with("team_id", &parent.config.team_id)
                .with("build_origin", "pause");
            self.node
                .mark_written_when_ready(disk.clone(), layer.rootfs_sealed.clone());
            span.in_scope(|| self.node.upload_filesystem(layer.clone(), object_metadata, disk));
        }
        let _ = reply.send(paused.map(|(_, snapshot)| snapshot));
    }

    async fn pause(
        &self,
        build_id: Uuid,
        template_id: String,
        span: Span,
        reply: oneshot::Sender<Result<Snapshot, String>>,
        disk: Option<Arc<PauseDiskReservation>>,
    ) {
        let parent = self.info.lock().clone();
        self.reclaim(&parent, false).await;
        let defer_rootfs = self.defers_rootfs(&parent);
        let snapshotted = self
            .node
            .snapshot_locally(
                &self.sandbox,
                &parent,
                build_id,
                defer_rootfs,
                SnapshotLineage::of_running_build(Origin::Pause, &parent.config.build_id, false),
            )
            .await;
        if snapshotted.is_ok() && self.pause_balloon.requested.load(Ordering::SeqCst) {
            *self.pause_balloon.totals.lock() = self.sandbox.flush_balloon_totals().await;
        }
        if snapshotted.is_ok() {
            self.node.events.publish(
                SandboxEvent::new(PAUSED, &event_sandbox(&parent))
                    .with_execution(&execution(&parent))
                    .with_pause_mode(false),
            );
        }
        if let Ok(local) = &snapshotted {
            let object_metadata = ObjectMetadata::default()
                .with("template_id", template_id)
                .with("team_id", &parent.config.team_id)
                .with("build_origin", "pause");
            self.node.mark_written_when_ready(disk.clone(), local.written());
            span.in_scope(|| self.node.upload(local.clone(), object_metadata.clone(), disk));
            self.node.harvest_after_pause(PausedBuild {
                parent: parent.clone(),
                build_id,
                object_metadata,
            });
        }
        let _ = reply.send(snapshotted.map(|local| local.snapshot));
    }

    async fn checkpoint(&self, build_id: Uuid, reply: oneshot::Sender<Result<LocalSnapshot, String>>) {
        let parent = self.info.lock().clone();
        self.reclaim(&parent, false).await;
        let _ = reply.send(
            self.node
                .snapshot_locally(
                    &self.sandbox,
                    &parent,
                    build_id,
                    false,
                    SnapshotLineage::of_running_build(Origin::Checkpoint, &parent.config.build_id, false),
                )
                .await,
        );
    }

    fn defers_rootfs(&self, parent: &SandboxInfo) -> bool {
        self.node
            .feature_flags
            .get(&DEFER_ROOTFS_EXPORT, &reclaim_contexts(&parent.config))
    }

    async fn reclaim(&self, parent: &SandboxInfo, filesystem_only: bool) {
        let contexts = reclaim_contexts(&parent.config);
        self.node
            .prepare_for_pause(
                &self.sandbox,
                PausePreparation {
                    contexts: &contexts,
                    filesystem_only,
                    use_case: USE_CASE_PAUSE,
                    firecracker_version: &parent.config.firecracker_version,
                },
            )
            .await;
    }

    async fn wait_for_ending(&mut self) -> Ending {
        loop {
            let deadline = self.deadline();
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(Command::Extend) => continue,
                    Some(Command::Trace { reply }) => drop(reply.send(self.sandbox.memory_trace())),
                    Some(Command::CheckpointInPlace { request, reply }) => {
                        let parent = self.info.lock().clone();
                        self.stop_checks().await;
                        self.reclaim(&parent, true).await;
                        let checkpointed = self.sandbox.checkpoint_filesystem_in_place(&request).await;
                        let lost = checkpointed.as_ref().err().is_some_and(|failure| failure.lost.is_some());
                        if !lost {
                            self.sandbox.guest_reclaim().unfreeze().await;
                            self.restart_checks();
                        }
                        drop(reply.send(checkpointed));
                        if lost {
                            return Ending::Stop { reason: "lost during an in-place checkpoint".to_owned() };
                        }
                    }
                    Some(Command::ResolveBalloonMode { reply }) => drop(reply.send(self.sandbox.resolve_balloon_mode().await)),
                    Some(Command::CheckpointMemoryInPlace { request, reply }) => {
                        let parent = self.info.lock().clone();
                        self.stop_checks().await;
                        self.reclaim(&parent, false).await;
                        let checkpointed = self.sandbox.checkpoint_memory_in_place(&request).await;
                        let lost = checkpointed.as_ref().err().is_some_and(|failure| failure.lost.is_some());
                        if !lost {
                            self.sandbox.guest_reclaim().unfreeze().await;
                            self.restart_checks();
                        }
                        drop(reply.send(checkpointed));
                        if lost {
                            return Ending::Stop { reason: "lost during an in-place checkpoint".to_owned() };
                        }
                    }
                    Some(Command::Checkpoint { build_id, reply }) => return Ending::Checkpoint { build_id, reply },
                    Some(Command::Pause { build_id, template_id, span, reply, disk, filesystem_only }) => return Ending::Pause { build_id, template_id, span, reply, disk, filesystem_only },
                    Some(Command::Delete) | None => return Ending::Stop { reason: DELETED.to_owned() },
                },
                () = expiry(deadline) => return Ending::Stop { reason: "expired".to_owned() },
                death = self.sandbox.died() => return Ending::Crashed { death },
            }
        }
    }

    async fn stop_checks(&mut self) {
        if let Some(health_loop) = self.health_loop.take() {
            health_loop.stop().await;
        }
        if let Some(hint_loop) = self.hint_loop.take() {
            hint_loop.stop().await;
        }
    }

    fn restart_checks(&mut self) {
        self.health_loop = self.health.clone().map(HealthLoop::start);
        self.hint_loop = HintLoop::start(&self.sandbox, &self.node, &self.info.lock(), self.checkpoints.clone());
    }

    fn ran_for(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.info.lock().started_at)
            .unwrap_or_default()
    }

    fn deadline(&self) -> Option<Instant> {
        let end = self.info.lock().end;
        self.node
            .local_expiry
            .then(|| Instant::now() + end.duration_since(SystemTime::now()).unwrap_or_default())
    }
}

fn network_release(node: Arc<Node>, lifecycle_id: String) -> Release {
    Box::new(move || {
        node.proxy
            .get()
            .iter()
            .for_each(|proxy| proxy.on_network_release(&lifecycle_id));
        std::future::ready(Ok(())).boxed()
    })
}

async fn expiry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
