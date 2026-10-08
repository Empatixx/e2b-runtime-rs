use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use super::client::FirecrackerClient;
use super::launch::Launch;

const READY_POLLS: u32 = 500;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(10);
const CONSOLE_TAIL: usize = 2000;

pub struct FirecrackerProcess {
    child: Child,
    socket: PathBuf,
    console: PathBuf,
}

impl FirecrackerProcess {
    pub async fn spawn(launch: &Launch) -> Result<Self, String> {
        remove_stale_socket(&launch.socket)?;
        let console = &launch.console;
        let console_file = File::create(console).map_err(|e| format!("create console {}: {e}", console.display()))?;
        let console_err = console_file
            .try_clone()
            .map_err(|e| format!("clone console {}: {e}", console.display()))?;
        let mut command = Command::new("setsid");
        command
            .args(["unshare", "-m", "--", "sh", "-c"])
            .arg(launch.script())
            .stdin(Stdio::null())
            .stdout(console_file)
            .stderr(console_err)
            .kill_on_drop(true);
        let cgroup_procs = launch
            .cgroup
            .as_deref()
            .map(|cgroup| join_cgroup_on_exec(&mut command, cgroup))
            .transpose()?;
        let child = command
            .spawn()
            .map_err(|e| format!("spawn firecracker {}: {e}", launch.binary.display()))?;
        drop(cgroup_procs);
        let mut process = Self {
            child,
            socket: launch.socket.clone(),
            console: console.clone(),
        };
        process.wait_until_ready().await?;
        Ok(process)
    }

    pub async fn exited(&mut self) -> String {
        let status = self
            .child
            .wait()
            .await
            .map_or_else(|e| format!("wait failed: {e}"), |status| status.to_string());
        format!("firecracker exited: {status}: {}", self.console_tail())
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn client(&self) -> FirecrackerClient {
        FirecrackerClient::new(self.socket.clone())
    }

    pub async fn kill(mut self) -> Result<(), String> {
        match self.child.try_wait() {
            Ok(Some(_)) => Ok(()),
            _ => self.child.kill().await.map_err(|e| format!("kill firecracker: {e}")),
        }
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

    pub fn console_tail(&self) -> String {
        let console = std::fs::read_to_string(&self.console).unwrap_or_default();
        let start = console.len().saturating_sub(CONSOLE_TAIL);
        console.get(start..).unwrap_or_default().trim().to_owned()
    }
}

fn join_cgroup_on_exec(command: &mut Command, cgroup: &Path) -> Result<File, String> {
    let path = cgroup.join("cgroup.procs");
    let procs = OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let fd = procs.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            nix::unistd::write(BorrowedFd::borrow_raw(fd), b"0")
                .map(|_| ())
                .map_err(io::Error::from)
        });
    }
    Ok(procs)
}

fn remove_stale_socket(socket: &Path) -> Result<(), String> {
    match std::fs::remove_file(socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("remove stale socket {}: {e}", socket.display())),
        _ => Ok(()),
    }
}
