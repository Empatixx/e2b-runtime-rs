use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use super::client::FirecrackerClient;

const READY_POLLS: u32 = 500;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CONSOLE_TAIL: usize = 2000;

pub struct FirecrackerProcess {
    child: Child,
    socket: PathBuf,
    console: PathBuf,
}

impl FirecrackerProcess {
    pub async fn spawn(binary: &Path, socket: &Path, namespace: &str, console: &Path) -> Result<Self, String> {
        remove_stale_socket(socket)?;
        let console_file = File::create(console).map_err(|e| format!("create console {}: {e}", console.display()))?;
        let console_err = console_file
            .try_clone()
            .map_err(|e| format!("clone console {}: {e}", console.display()))?;
        let child = Command::new("ip")
            .args(["netns", "exec", namespace])
            .arg(binary)
            .arg("--api-sock")
            .arg(socket)
            .stdin(Stdio::null())
            .stdout(console_file)
            .stderr(console_err)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn firecracker {}: {e}", binary.display()))?;
        let mut process = Self {
            child,
            socket: socket.to_path_buf(),
            console: console.to_path_buf(),
        };
        process.wait_until_ready().await?;
        Ok(process)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn client(&self) -> FirecrackerClient {
        FirecrackerClient::new(self.socket.clone())
    }

    pub async fn kill(mut self) -> Result<(), String> {
        self.child.kill().await.map_err(|e| format!("kill firecracker: {e}"))
    }

    async fn wait_until_ready(&mut self) -> Result<(), String> {
        for _ in 0..READY_POLLS {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| format!("wait for firecracker: {e}"))?
            {
                return Err(format!("firecracker exited: {status}: {}", self.console_tail()));
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

    fn console_tail(&self) -> String {
        let console = std::fs::read_to_string(&self.console).unwrap_or_default();
        let start = console.len().saturating_sub(CONSOLE_TAIL);
        console.get(start..).unwrap_or_default().trim().to_owned()
    }
}

fn remove_stale_socket(socket: &Path) -> Result<(), String> {
    match std::fs::remove_file(socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("remove stale socket {}: {e}", socket.display())),
        _ => Ok(()),
    }
}
