use std::io::Write;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::envd::process::{CommandContext, run_command};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SANDBOX_BUSYBOX: &str = "/usr/bin/busybox";
const SANDBOX_SSH_TARGET: &str = "root@169.254.0.21";

#[derive(Debug, Clone)]
pub struct Guest {
    pub envd: SocketAddr,
    pub access_token: String,
    pub namespace: String,
}

impl Guest {
    pub async fn run(&self, command: &str) -> Result<(), String> {
        let context = CommandContext {
            user: "root".to_owned(),
            access_token: Some(self.access_token.clone()),
            timeout: Some(COMMAND_TIMEOUT),
            ..CommandContext::default()
        };
        run_command(self.envd, command, &context, |stdout, stderr| {
            print!("{stdout}{stderr}");
            let _ = std::io::stdout().flush();
        })
        .await
        .map_err(|e| match e.strip_prefix("exit status ") {
            Some(code) => format!("command exited with code {code}"),
            None => e,
        })
    }

    pub fn run_in_background(&self, command: &str) -> oneshot::Receiver<Result<(), String>> {
        let (done, finished) = oneshot::channel();
        let guest = self.clone();
        let command = command.to_owned();
        tokio::spawn(async move { drop(done.send(guest.run(&command).await)) });
        finished
    }

    pub async fn sync_and_drop_caches(&self) -> Result<(), String> {
        self.run(&format!("{SANDBOX_BUSYBOX} sync"))
            .await
            .map_err(|e| format!("sync failed: {e}"))?;
        self.run("echo 3 > /proc/sys/vm/drop_caches")
            .await
            .map_err(|e| format!("drop_caches failed: {e}"))
    }

    pub fn ssh_hint(&self) -> String {
        format!(
            "   sudo nsenter --net=/var/run/netns/{} ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no {SANDBOX_SSH_TARGET}",
            self.namespace
        )
    }

    pub async fn wait_for_pause_signal(
        &self,
        name: &str,
        command: Option<oneshot::Receiver<Result<(), String>>>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let kind = parse_signal(name).ok_or_else(|| format!("unknown signal: {name}"))?;
        println!("⏳ Waiting for {name} signal...");
        println!("{}", self.ssh_hint());
        let mut signals = signal(kind).map_err(|e| format!("listen for {name}: {e}"))?;
        let mut command = command;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return Err("context canceled".to_owned()),
                _ = signals.recv() => {
                    println!("📨 Received {name} signal");
                    return Ok(());
                }
                finished = async {
                    match command.as_mut() {
                        Some(running) => running.await,
                        None => std::future::pending().await,
                    }
                } => {
                    match finished.unwrap_or(Ok(())) {
                        Ok(()) => println!("ℹ️  Command completed before pause signal"),
                        Err(e) => println!("⚠️  Command exited before pause signal: {e}"),
                    }
                    command = None;
                }
            }
        }
    }
}

pub fn parse_signal(name: &str) -> Option<SignalKind> {
    let upper = name.to_uppercase();
    match upper.strip_prefix("SIG").unwrap_or(&upper) {
        "TERM" => Some(SignalKind::terminate()),
        "USR1" => Some(SignalKind::user_defined1()),
        "USR2" => Some(SignalKind::user_defined2()),
        "HUP" => Some(SignalKind::hangup()),
        "INT" => Some(SignalKind::interrupt()),
        "QUIT" => Some(SignalKind::quit()),
        "CONT" => Some(SignalKind::from_raw(nix::libc::SIGCONT)),
        "WINCH" => Some(SignalKind::window_change()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_parse_with_or_without_the_prefix_like_go() {
        assert_eq!(parse_signal("SIGUSR1"), Some(SignalKind::user_defined1()));
        assert_eq!(parse_signal("term"), Some(SignalKind::terminate()));
        assert_eq!(parse_signal("KILL"), None);
    }
}
