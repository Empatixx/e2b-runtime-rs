use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};

use super::FirecrackerApi;

const SOCKET: &str = "fc.sock";
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_POLL: Duration = Duration::from_millis(1);

pub struct FirecrackerProcess {
    child: Child,
    pub pid: u32,
    pub api: FirecrackerApi,
}

impl FirecrackerProcess {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn spawn(binary: &Path, namespace: &str, dir: &Path) -> Result<Self> {
        let socket = dir.join(SOCKET);
        let _ = std::fs::remove_file(&socket);
        let log = std::fs::File::create(dir.join("firecracker.log"))?;
        let child = Command::new("ip")
            .args(["netns", "exec", namespace])
            .arg(binary)
            .args(["--api-sock", SOCKET])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .context("spawn firecracker")?;
        let pid = child.id().context("firecracker exited at once")?;
        wait_for_socket(&socket).await?;
        Ok(Self {
            child,
            pid,
            api: FirecrackerApi::new(socket),
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn kill(mut self) {
        let _ = self.child.kill().await;
    }
}

async fn wait_for_socket(socket: &Path) -> Result<()> {
    let deadline = Instant::now() + SOCKET_TIMEOUT;
    while !socket.exists() {
        if Instant::now() > deadline {
            bail!("firecracker socket {} did not appear", socket.display());
        }
        sleep(SOCKET_POLL).await;
    }
    Ok(())
}
