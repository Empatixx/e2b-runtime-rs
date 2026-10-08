use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use super::node::Node;
use super::registry::Lifecycle;
use super::status::Work;
use crate::network::slot::Slot;
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;
use crate::sandbox::snapshot::Snapshot;
use crate::storage::backend::ObjectMetadata;
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
    pub started_at: SystemTime,
}

pub enum Command {
    Delete,
    Pause {
        build_id: Uuid,
        template_id: String,
        reply: oneshot::Sender<Result<Snapshot, String>>,
    },
    Extend,
}

#[derive(Clone)]
pub struct SandboxHandle {
    pub info: Arc<Mutex<SandboxInfo>>,
    pub commands: mpsc::Sender<Command>,
}

enum Ending {
    Stop {
        reason: String,
    },
    Pause {
        build_id: Uuid,
        template_id: String,
        reply: oneshot::Sender<Result<Snapshot, String>>,
    },
}

struct Running {
    sandbox: Sandbox,
    slot: Slot,
    info: Arc<Mutex<SandboxInfo>>,
    commands: mpsc::Receiver<Command>,
    node: Arc<Node>,
    _work: Work,
}

pub fn spawn(sandbox: Sandbox, slot: Slot, info: SandboxInfo, node: Arc<Node>) -> SandboxHandle {
    let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
    let handle = SandboxHandle {
        info: Arc::new(Mutex::new(info)),
        commands,
    };
    let running = Running {
        sandbox,
        slot,
        info: handle.info.clone(),
        commands: receiver,
        _work: node.status.track_work(),
        node,
    };
    tokio::spawn(running.run());
    handle
}

impl Running {
    async fn run(mut self) {
        let id = self.sandbox.id().to_owned();
        let ending = self.wait_for_ending().await;
        self.node.registry.stop(&id);
        match ending {
            Ending::Pause {
                build_id,
                template_id,
                reply,
            } => self.pause(build_id, template_id, reply).await,
            Ending::Stop { reason } => tracing::info!(sandbox = %id, %reason, "stopping sandbox"),
        }
        if let Err(e) = self.sandbox.stop().await {
            tracing::error!(sandbox = %id, error = %e, "sandbox did not stop cleanly");
        }
        let _ = std::fs::remove_dir_all(self.node.work_dir_of(&id));
        self.node.network.return_slot(self.slot);
        let lifecycle = self.info.lock().lifecycle.clone();
        self.node.registry.finish(&lifecycle);
    }

    async fn pause(&self, build_id: Uuid, template_id: String, reply: oneshot::Sender<Result<Snapshot, String>>) {
        let parent = self.info.lock().clone();
        let snapshotted = self.node.snapshot_locally(&self.sandbox, &parent, build_id).await;
        if let Ok(local) = &snapshotted {
            let object_metadata = ObjectMetadata::default()
                .with("template_id", template_id)
                .with("team_id", &parent.config.team_id)
                .with("build_origin", "pause");
            self.node.upload(local.clone(), object_metadata);
        }
        let _ = reply.send(snapshotted.map(|local| local.snapshot));
    }

    async fn wait_for_ending(&mut self) -> Ending {
        loop {
            let deadline = self.deadline();
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(Command::Extend) => continue,
                    Some(Command::Pause { build_id, template_id, reply }) => return Ending::Pause { build_id, template_id, reply },
                    Some(Command::Delete) | None => return Ending::Stop { reason: "deleted".to_owned() },
                },
                () = expiry(deadline) => return Ending::Stop { reason: "expired".to_owned() },
                reason = self.sandbox.died() => return Ending::Stop { reason },
            }
        }
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
