use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use super::node::Node;
use crate::network::slot::Slot;
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;
use crate::sandbox::snapshot::Snapshot;

const COMMAND_QUEUE: usize = 8;

#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub config: SandboxConfig,
    pub start: SystemTime,
    pub end: SystemTime,
}

pub enum Command {
    Delete,
    Pause {
        build_id: Uuid,
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
        reply: oneshot::Sender<Result<Snapshot, String>>,
    },
}

pub fn spawn(sandbox: Sandbox, slot: Slot, info: SandboxInfo, node: Arc<Node>) -> SandboxHandle {
    let (commands, receiver) = mpsc::channel(COMMAND_QUEUE);
    let handle = SandboxHandle {
        info: Arc::new(Mutex::new(info)),
        commands,
    };
    tokio::spawn(run(sandbox, slot, handle.info.clone(), receiver, node));
    handle
}

async fn run(
    mut sandbox: Sandbox,
    slot: Slot,
    info: Arc<Mutex<SandboxInfo>>,
    mut commands: mpsc::Receiver<Command>,
    node: Arc<Node>,
) {
    let id = sandbox.id().to_owned();
    let ending = loop {
        let deadline = deadline_of(info.lock().end);
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Extend) => continue,
                Some(Command::Pause { build_id, reply }) => break Ending::Pause { build_id, reply },
                Some(Command::Delete) | None => break Ending::Stop { reason: "deleted".to_owned() },
            },
            () = tokio::time::sleep_until(deadline) => break Ending::Stop { reason: "expired".to_owned() },
            reason = sandbox.died() => break Ending::Stop { reason },
        }
    };
    node.registry.remove(&id);
    match ending {
        Ending::Pause { build_id, reply } => drop(reply.send(sandbox.pause(&node.storage, build_id).await)),
        Ending::Stop { reason } => {
            tracing::info!(sandbox = %id, %reason, "stopping sandbox");
            if let Err(e) = sandbox.stop().await {
                tracing::error!(sandbox = %id, error = %e, "sandbox did not stop cleanly");
            }
        }
    }
    node.slots.release(slot);
}

fn deadline_of(end: SystemTime) -> Instant {
    Instant::now() + end.duration_since(SystemTime::now()).unwrap_or_default()
}
