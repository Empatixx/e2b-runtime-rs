use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use parking_lot::{Mutex, MutexGuard};

use super::scan::{Listener, local_listeners};
use crate::cgroups::Placement;
use crate::proto::upgrade::ForwardedPort;

const SCAN_INTERVAL: Duration = Duration::from_millis(1000);
const GATEWAY_IP: &str = "169.254.0.21";

pub struct Forward {
    socat_pid: Option<u32>,
    listener: Listener,
}

#[derive(Clone, Default)]
pub struct Forwards {
    inner: Arc<Mutex<HashMap<String, Forward>>>,
}

pub struct Forwarder {
    placement: Option<Placement>,
    forwards: Forwards,
}

impl Forwards {
    pub fn lock(&self) -> MutexGuard<'_, HashMap<String, Forward>> {
        self.inner.lock()
    }

    pub fn import(&self, forwards: &[ForwardedPort]) -> usize {
        let mut map = self.inner.lock();
        forwards
            .iter()
            .filter(|forward| forward.socat_pid > 0)
            .filter(|forward| kill(Pid::from_raw(forward.socat_pid), None).is_ok())
            .map(|forward| {
                map.insert(
                    forward.key.clone(),
                    Forward {
                        socat_pid: Some(forward.socat_pid as u32),
                        listener: Listener {
                            pid: forward.listener_pid,
                            ip: String::new(),
                            port: forward.port,
                            family: forward.family,
                        },
                    },
                );
                let pid = Pid::from_raw(forward.socat_pid);
                tokio::task::spawn_blocking(move || {
                    let _ = nix::sys::wait::waitpid(pid, None);
                });
            })
            .count()
    }
}

pub fn export(forwards: &HashMap<String, Forward>) -> Vec<ForwardedPort> {
    forwards
        .iter()
        .filter_map(|(key, forward)| {
            forward.socat_pid.map(|socat_pid| ForwardedPort {
                key: key.clone(),
                port: forward.listener.port,
                listener_pid: forward.listener.pid,
                family: forward.listener.family,
                socat_pid: socat_pid as i32,
                ..Default::default()
            })
        })
        .collect()
}

impl Forwarder {
    pub fn new(placement: Option<Placement>, forwards: Forwards) -> Self {
        Self { placement, forwards }
    }

    pub async fn run(self) {
        let mut ticks = tokio::time::interval(SCAN_INTERVAL);
        loop {
            ticks.tick().await;
            let listeners = tokio::task::spawn_blocking(|| local_listeners(Path::new("/proc")))
                .await
                .unwrap_or_default();
            self.reconcile(listeners);
        }
    }

    fn reconcile(&self, listeners: Vec<Listener>) {
        let mut forwards = self.forwards.lock();
        let current: HashMap<String, Listener> = listeners
            .into_iter()
            .map(|listener| (format!("{}-{}", listener.pid, listener.port), listener))
            .collect();
        for (key, listener) in &current {
            if !forwards.contains_key(key) {
                tracing::debug!(ip = %listener.ip, port = listener.port, family = listener.family, state = "LISTEN", "Detected new opened port on localhost that is not forwarded");
                let socat_pid = self.start(listener);
                forwards.insert(
                    key.clone(),
                    Forward {
                        socat_pid,
                        listener: listener.clone(),
                    },
                );
            }
        }
        forwards
            .iter_mut()
            .filter(|(key, _)| !current.contains_key(*key))
            .for_each(|(_, forward)| stop(forward));
    }

    fn start(&self, listener: &Listener) -> Option<u32> {
        let args = socat_args(listener);
        let mut command = tokio::process::Command::new("socat");
        command.args(&args).process_group(0);
        if let Some(procs) = self.placement.as_ref().map(Placement::raw_fd) {
            unsafe { command.pre_exec(move || Placement::join(procs)) };
        }
        let line = format!("socat {}", args.join(" "));
        tracing::debug!(socat_cmd = %line, pid = listener.pid, family = listener.family, source_ip = GATEWAY_IP, port = listener.port, "About to start port forwarding");
        match command.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                tokio::spawn(async move {
                    match child.wait().await {
                        Ok(status) if !status.success() => {
                            tracing::debug!(socat_cmd = %line, error = %status, "Port forwarding socat process exited")
                        }
                        Err(e) => {
                            tracing::debug!(socat_cmd = %line, error = %e, "Port forwarding socat process exited")
                        }
                        Ok(_) => {}
                    }
                });
                pid
            }
            Err(e) => {
                tracing::error!(socat_cmd = %line, error = %e, "Failed to start port forwarding - failed to start socat");
                None
            }
        }
    }
}

fn stop(forward: &mut Forward) {
    let Some(pid) = forward.socat_pid.take() else {
        return;
    };
    tracing::debug!(
        socat_pid = pid,
        pid = forward.listener.pid,
        port = forward.listener.port,
        "Stopping port forwarding"
    );
    match kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL) {
        Ok(()) => tracing::debug!(socat_pid = pid, "Stopped port forwarding"),
        Err(e) => tracing::error!(socat_pid = pid, error = %e, "Failed to kill process group"),
    }
}

fn socat_args(listener: &Listener) -> Vec<String> {
    vec![
        "-d".to_owned(),
        "-d".to_owned(),
        "-d".to_owned(),
        format!("TCP4-LISTEN:{},bind={GATEWAY_IP},reuseaddr,fork", listener.port),
        format!("TCP{}:localhost:{}", listener.family, listener.port),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socat_bridges_the_gateway_address_to_the_local_listener() {
        let listener = Listener {
            pid: 7,
            ip: "::1".to_owned(),
            port: 3000,
            family: 6,
        };
        assert_eq!(
            socat_args(&listener),
            [
                "-d",
                "-d",
                "-d",
                "TCP4-LISTEN:3000,bind=169.254.0.21,reuseaddr,fork",
                "TCP6:localhost:3000"
            ]
        );
    }
}
