use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use parking_lot::{Mutex, MutexGuard};

use super::scan::{Listener, local_listeners};
use crate::cgroups::freeze::WorkloadFreezer;
use crate::cgroups::{Placement, ProcessType};
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
    freezer: Arc<WorkloadFreezer>,
    forwards: Forwards,
}

enum Socat {
    Running(u32),
    Failed,
    Refused,
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
                let pid = forward.socat_pid;
                tokio::task::spawn_blocking(move || crate::reaper::reap(pid));
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
    pub fn new(freezer: Arc<WorkloadFreezer>, forwards: Forwards) -> Self {
        Self { freezer, forwards }
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
                let socat_pid = match self.start(listener) {
                    Socat::Running(pid) => Some(pid),
                    Socat::Failed => None,
                    Socat::Refused => continue,
                };
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

    fn start(&self, listener: &Listener) -> Socat {
        let args = socat_args(listener);
        let mut command = tokio::process::Command::new("socat");
        command.args(&args).process_group(0);
        if let Some(procs) = self
            .freezer
            .manager()
            .placement(ProcessType::Socat)
            .as_ref()
            .map(Placement::raw_fd)
        {
            unsafe { command.pre_exec(move || Placement::join(procs)) };
        }
        let line = format!("socat {}", args.join(" "));
        tracing::debug!(socat_cmd = %line, pid = listener.pid, family = listener.family, source_ip = GATEWAY_IP, port = listener.port, "About to start port forwarding");
        if let Err(refused) = self.freezer.probe_thawed(ProcessType::Socat) {
            tracing::debug!(port = listener.port, error = %refused, "Deferring port forwarding: the socat cgroup is frozen");
            return Socat::Refused;
        }
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
                pid.map_or(Socat::Failed, Socat::Running)
            }
            Err(e) => {
                tracing::error!(socat_cmd = %line, error = %e, "Failed to start port forwarding - failed to start socat");
                Socat::Failed
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
    use crate::cgroups::CgroupManager;

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

    fn frozen_socat_forwarder(root: &Path) -> Forwarder {
        let socats = root.join("socats");
        std::fs::create_dir_all(&socats).unwrap();
        std::fs::write(socats.join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        let manager = CgroupManager::at_paths(root, &[(ProcessType::Socat, socats)]);
        Forwarder::new(WorkloadFreezer::new(Arc::new(manager)), Forwards::default())
    }

    fn listener_on(port: u32) -> Listener {
        Listener {
            pid: 4242,
            ip: "127.0.0.1".to_owned(),
            port,
            family: 4,
        }
    }

    #[tokio::test]
    async fn a_port_skipped_while_the_socat_cgroup_is_frozen_is_retried_on_the_next_scan() {
        let root = tempfile::tempdir().unwrap();
        let forwarder = frozen_socat_forwarder(root.path());
        let key = "4242-65000";
        let tracked = |forwarder: &Forwarder| forwarder.forwards.lock().contains_key(key);

        forwarder.reconcile(vec![listener_on(65000)]);
        forwarder.reconcile(vec![listener_on(65000)]);
        assert!(
            !tracked(&forwarder),
            "a forward refused while the cgroup is frozen must not be recorded"
        );

        std::fs::write(root.path().join("socats/cgroup.events"), "populated 1\nfrozen 0\n").unwrap();

        forwarder.reconcile(vec![listener_on(65000)]);
        assert!(
            tracked(&forwarder),
            "once the cgroup is thawed the next scan must attempt the forward again"
        );
        forwarder.forwards.lock().values_mut().for_each(stop);
    }
}
