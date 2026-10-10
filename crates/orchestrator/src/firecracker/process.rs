mod child;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use self::child::{Child, Spawned, find_in_path};
use super::client::FirecrackerClient;
use super::console::ConsoleTap;
use super::exit::ExitInfo;
use super::launch::Launch;
use super::metrics::FirecrackerMetrics;

const READY_POLLS: u32 = 5000;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(1);
const CONSOLE_DRAIN: Duration = Duration::from_secs(1);

pub struct FirecrackerExit {
    pub message: String,
    pub info: Option<ExitInfo>,
}

pub struct FirecrackerProcess {
    child: Child,
    socket: PathBuf,
    console_tap: ConsoleTap,
    drained: Option<JoinHandle<()>>,
    metrics: Option<FirecrackerMetrics>,
}

impl FirecrackerProcess {
    pub async fn spawn(launch: &Launch) -> Result<Self, String> {
        remove_stale_socket(&launch.socket)?;
        let Spawned { child, stdout, stderr } =
            start_in_cgroup(launch).map_err(|e| format!("spawn firecracker {}: {e}", launch.binary.display()))?;
        let console_tap = ConsoleTap::open();
        let drained = Some(console_tap.pump(stdout, stderr));
        let mut process = Self {
            child,
            socket: launch.socket.clone(),
            console_tap,
            drained,
            metrics: None,
        };
        process.wait_until_ready().await?;
        Ok(process)
    }

    pub async fn exited(&mut self) -> FirecrackerExit {
        let status = self.child.wait().await;
        let described = status
            .as_ref()
            .map_or_else(|e| format!("wait failed: {e}"), |status| status.to_string());
        FirecrackerExit {
            message: format!("firecracker exited: {described}: {}", self.drained_tail().await),
            info: status.ok().and_then(ExitInfo::from_status),
        }
    }

    pub fn console_lines(&self) -> UnboundedReceiver<String> {
        self.console_tap.subscribe()
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn client(&self) -> FirecrackerClient {
        FirecrackerClient::new(self.socket.clone())
    }

    pub async fn start_metrics(&mut self, fifo: &Path, sandbox_id: &str) -> Result<(), String> {
        let metrics = FirecrackerMetrics::start(fifo, self.client(), sandbox_id).await?;
        self.metrics = Some(metrics);
        Ok(())
    }

    pub fn metrics(&self) -> Option<&FirecrackerMetrics> {
        self.metrics.as_ref()
    }

    pub async fn kill(mut self) -> Result<(), String> {
        let killed = match self.child.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => self.child.kill().await.map_err(|e| format!("kill firecracker: {e}")),
        };
        if let Err(e) = &killed {
            tracing::error!(error = %e, "firecracker may still be running and holding the rootfs device");
        }
        let _ = std::fs::remove_file(&self.socket);
        killed
    }

    async fn wait_until_ready(&mut self) -> Result<(), String> {
        for _ in 0..READY_POLLS {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| format!("wait for firecracker: {e}"))?
            {
                return Err(format!("firecracker exited: {status}: {}", self.drained_tail().await));
            }
            if UnixStream::connect(&self.socket).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
        Err(format!(
            "firecracker api socket {} did not accept connections: {}",
            self.socket.display(),
            self.console_tail()
        ))
    }

    async fn drained_tail(&mut self) -> String {
        if let Some(drained) = self.drained.take() {
            let _ = tokio::time::timeout(CONSOLE_DRAIN, drained).await;
        }
        self.console_tail()
    }

    pub fn console_tail(&self) -> String {
        self.console_tap.tail()
    }
}

fn start_in_cgroup(launch: &Launch) -> io::Result<Spawned> {
    let script = launch.script();
    child::spawn(
        &find_in_path("unshare")?,
        &["-m", "--", "sh", "-c", &script],
        launch.cgroup.as_deref(),
    )
}

fn remove_stale_socket(socket: &Path) -> Result<(), String> {
    match std::fs::remove_file(socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("remove stale socket {}: {e}", socket.display())),
        _ => Ok(()),
    }
}
