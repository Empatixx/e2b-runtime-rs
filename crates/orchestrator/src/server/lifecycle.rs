use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use super::host_stats::HostStatsCollector;
use super::node::prefetch::PausedBuild;
use super::node::{Node, event_sandbox, execution, stats_identity};
use super::oom_monitor::{OomMonitor, SharedWatermark};
use super::registry::Lifecycle;
use super::status::Work;
use crate::events::{PAUSED, SandboxEvent};
use crate::memory::trace::Trace;
use crate::network::slot::Slot;
use crate::proto::{SandboxConfig, SchedulingMetadata};
use crate::sandbox::Sandbox;
use crate::sandbox::snapshot::{LocalSnapshot, Snapshot};
use crate::storage::backend::ObjectMetadata;
use crate::telemetry::server::sandbox::{record_crash, record_execution};
use crate::template::Template;
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
}

pub enum Command {
    Delete,
    Pause {
        build_id: Uuid,
        template_id: String,
        reply: oneshot::Sender<Result<Snapshot, String>>,
    },
    Checkpoint {
        build_id: Uuid,
        reply: oneshot::Sender<Result<LocalSnapshot, String>>,
    },
    Trace {
        reply: oneshot::Sender<Option<Trace>>,
    },
    Extend,
}

#[derive(Clone)]
pub struct SandboxHandle {
    pub info: Arc<Mutex<SandboxInfo>>,
    pub commands: mpsc::Sender<Command>,
    pub updates: Arc<tokio::sync::Mutex<()>>,
    cleanup: Arc<AtomicBool>,
}

impl SandboxHandle {
    pub fn lifecycle(&self) -> Lifecycle {
        self.info.lock().lifecycle.clone()
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
        reason: String,
    },
    Pause {
        build_id: Uuid,
        template_id: String,
        reply: oneshot::Sender<Result<Snapshot, String>>,
    },
    Checkpoint {
        build_id: Uuid,
        reply: oneshot::Sender<Result<LocalSnapshot, String>>,
    },
}

const STOP_REASON_KILLED: &str = "killed";
const STOP_REASON_PAUSED: &str = "paused";
const STOP_REASON_CHECKPOINTING: &str = "checkpointing";
const STOP_REASON_CRASHED: &str = "crashed";
const CRASH_CAUSE_MEMORY_HANDLER_FAILED: &str = "memory_handler_failed";
const CRASH_CAUSE_UNKNOWN: &str = "unknown";

impl Ending {
    fn stop_reason(&self) -> &'static str {
        match self {
            Self::Stop { .. } => STOP_REASON_KILLED,
            Self::Crashed { .. } => STOP_REASON_CRASHED,
            Self::Pause { .. } => STOP_REASON_PAUSED,
            Self::Checkpoint { .. } => STOP_REASON_CHECKPOINTING,
        }
    }

    fn record_metrics(&self, ran_for: Duration) {
        record_execution(ran_for, self.stop_reason());
        if let Self::Crashed { reason } = self {
            record_crash(crash_cause(reason));
        }
    }
}

fn crash_cause(reason: &str) -> &'static str {
    match reason.contains("memory server") {
        true => CRASH_CAUSE_MEMORY_HANDLER_FAILED,
        false => CRASH_CAUSE_UNKNOWN,
    }
}

struct Running {
    sandbox: Sandbox,
    slot: Slot,
    info: Arc<Mutex<SandboxInfo>>,
    cleanup: Arc<AtomicBool>,
    commands: mpsc::Receiver<Command>,
    node: Arc<Node>,
    host_stats: Option<HostStatsCollector>,
    _oom: OomMonitor,
    _work: Work,
}

pub fn spawn(sandbox: Sandbox, slot: Slot, info: SandboxInfo, node: Arc<Node>, throwaway: bool) -> SandboxHandle {
    let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
    let handle = SandboxHandle {
        info: Arc::new(Mutex::new(info)),
        commands,
        updates: Arc::default(),
        cleanup: Arc::default(),
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
    let oom = OomMonitor::start(
        sandbox.envd_address(),
        access_token,
        sandbox.id().to_owned(),
        oom_watermark,
    );
    let running = Running {
        host_stats,
        _oom: oom,
        sandbox,
        slot,
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
    async fn run(mut self) {
        let ending = self.wait_for_ending().await;
        let lifecycle = self.info.lock().lifecycle.clone();
        ending.record_metrics(self.ran_for());
        let span = tracing::info_span!(parent: None, "stop sandbox-lifecycle");
        self.tear_down(ending, lifecycle).instrument(span).await;
    }

    async fn tear_down(mut self, ending: Ending, lifecycle: Lifecycle) {
        let id = self.sandbox.id().to_owned();
        self.cleanup.store(true, Ordering::SeqCst);
        self.node.mark_stopping(&lifecycle).await;
        match ending {
            Ending::Pause {
                build_id,
                template_id,
                reply,
            } => self.pause(build_id, template_id, reply).await,
            Ending::Checkpoint { build_id, reply } => self.checkpoint(build_id, reply).await,
            Ending::Stop { reason } | Ending::Crashed { reason } => {
                tracing::info!(sandbox = %id, %reason, "stopping sandbox")
            }
        }
        if let Some(collector) = self.host_stats.take() {
            collector.stop().await;
        }
        let stopped = self.sandbox.stop().await;
        let _ = std::fs::remove_dir_all(self.node.work_dir_of(&lifecycle));
        match stopped {
            Ok(()) => self.node.network.return_slot(self.slot),
            Err(e) => {
                tracing::error!(sandbox = %id, error = %e, "sandbox did not stop cleanly");
                self.node.network.discard_slot(self.slot);
            }
        }
        self.node.registry.finish(&lifecycle);
    }

    async fn pause(&self, build_id: Uuid, template_id: String, reply: oneshot::Sender<Result<Snapshot, String>>) {
        let parent = self.info.lock().clone();
        let snapshotted = self.node.snapshot_locally(&self.sandbox, &parent, build_id).await;
        if snapshotted.is_ok() {
            self.node
                .events
                .publish(SandboxEvent::new(PAUSED, &event_sandbox(&parent)).with_execution(&execution(&parent)));
        }
        if let Ok(local) = &snapshotted {
            let object_metadata = ObjectMetadata::default()
                .with("template_id", template_id)
                .with("team_id", &parent.config.team_id)
                .with("build_origin", "pause");
            self.node.upload(local.clone(), object_metadata.clone());
            self.node
                .harvest_after_pause(PausedBuild {
                    parent: parent.clone(),
                    build_id,
                    object_metadata,
                    memfile_ready: local.memfile_ready.clone(),
                })
                .await;
        }
        let _ = reply.send(snapshotted.map(|local| local.snapshot));
    }

    async fn checkpoint(&self, build_id: Uuid, reply: oneshot::Sender<Result<LocalSnapshot, String>>) {
        let parent = self.info.lock().clone();
        let _ = reply.send(self.node.snapshot_locally(&self.sandbox, &parent, build_id).await);
    }

    async fn wait_for_ending(&mut self) -> Ending {
        loop {
            let deadline = self.deadline();
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(Command::Extend) => continue,
                    Some(Command::Trace { reply }) => drop(reply.send(self.sandbox.memory_trace())),
                    Some(Command::Checkpoint { build_id, reply }) => return Ending::Checkpoint { build_id, reply },
                    Some(Command::Pause { build_id, template_id, reply }) => return Ending::Pause { build_id, template_id, reply },
                    Some(Command::Delete) | None => return Ending::Stop { reason: "deleted".to_owned() },
                },
                () = expiry(deadline) => return Ending::Stop { reason: "expired".to_owned() },
                reason = self.sandbox.died() => return Ending::Crashed { reason },
            }
        }
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

async fn expiry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
