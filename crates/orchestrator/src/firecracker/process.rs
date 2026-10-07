use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

use super::client::FirecrackerClient;

const SOCKET_POLLS: u32 = 500;
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub struct FirecrackerProcess {
    child: Child,
    socket: PathBuf,
}

impl FirecrackerProcess {
    pub async fn spawn(binary: &Path, socket: &Path, namespace: &str, console: &Path) -> Result<Self, String> {
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
        let process = Self {
            child,
            socket: socket.to_path_buf(),
        };
        process.wait_for_socket().await?;
        Ok(process)
    }

    pub fn pid(&self) -> u32 {
        self.child.id().unwrap_or_default()
    }

    pub fn client(&self) -> FirecrackerClient {
        FirecrackerClient::new(self.socket.clone())
    }

    pub async fn kill(mut self) -> Result<(), String> {
        self.child.kill().await.map_err(|e| format!("kill firecracker: {e}"))
    }

    async fn wait_for_socket(&self) -> Result<(), String> {
        for _ in 0..SOCKET_POLLS {
            if self.socket.exists() {
                return Ok(());
            }
            tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
        }
        Err(format!(
            "firecracker api socket {} did not appear",
            self.socket.display()
        ))
    }
}
