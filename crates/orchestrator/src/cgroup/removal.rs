use std::io;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Cgroup;

pub const CONTEXT_CANCELED: &str = "context canceled";
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const RMDIR_DEADLINE: Duration = Duration::from_secs(2);
const RMDIR_RETRY_INTERVAL: Duration = Duration::from_millis(200);

pub fn parse_events_populated(events: &str) -> Result<bool, String> {
    events
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find_map(|fields| match fields.as_slice() {
            ["populated", value] => Some(match *value {
                "0" => Ok(false),
                "1" => Ok(true),
                other => Err(format!("invalid populated value in cgroup.events: {other:?}")),
            }),
            _ => None,
        })
        .unwrap_or_else(|| Err("missing populated value in cgroup.events".to_owned()))
}

fn is_gone(result: &io::Result<()>) -> bool {
    result
        .as_ref()
        .map_or_else(|e| e.kind() == io::ErrorKind::NotFound, |()| true)
}

impl Cgroup {
    pub async fn remove(&mut self, cancel: &CancellationToken) -> Result<(), String> {
        if self.removed {
            return Ok(());
        }
        self.peak.take();
        let first = self.rmdir();
        if is_gone(&first) {
            self.removed = true;
            tracing::debug!(cgroup_path = %self.path.display(), "removed cgroup for sandbox");
            return Ok(());
        }
        tracing::warn!(
            cgroup_path = %self.path.display(),
            error = %first.err().map(|e| e.to_string()).unwrap_or_default(),
            "cgroup rmdir failed, falling back to cgroup.kill"
        );
        if let Err(e) = self.kill(cancel).await {
            tracing::warn!(cgroup_path = %self.path.display(), error = %e, "failed to kill cgroup processes");
        }
        self.rmdir_until_gone(cancel).await?;
        self.removed = true;
        tracing::debug!(cgroup_path = %self.path.display(), "removed cgroup for sandbox after cgroup.kill");
        Ok(())
    }

    async fn rmdir_until_gone(&self, cancel: &CancellationToken) -> Result<(), String> {
        let deadline = Instant::now() + RMDIR_DEADLINE;
        loop {
            let attempt = self.rmdir();
            if is_gone(&attempt) {
                return Ok(());
            }
            if let Err(e) = attempt
                && Instant::now() > deadline
            {
                return Err(format!("failed to remove cgroup: {e}"));
            }
            tokio::select! {
                () = cancel.cancelled() => return Err(CONTEXT_CANCELED.to_owned()),
                () = tokio::time::sleep(RMDIR_RETRY_INTERVAL) => {}
            }
        }
    }

    fn rmdir(&self) -> io::Result<()> {
        self.remove_vcpu_cgroup()?;
        std::fs::remove_dir(&self.path)
    }

    fn populated(&self) -> Result<bool, String> {
        match std::fs::read_to_string(self.path.join("cgroup.events")) {
            Ok(events) => parse_events_populated(&events),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("failed to read cgroup.events: {e}")),
        }
    }

    async fn kill(&self, cancel: &CancellationToken) -> Result<(), String> {
        if !self.populated()? {
            return Ok(());
        }
        match std::fs::write(self.path.join("cgroup.kill"), "1") {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("failed to write cgroup.kill: {e}")),
        }
        let deadline = Instant::now() + KILL_TIMEOUT;
        while self.populated()? {
            if cancel.is_cancelled() {
                return Err(CONTEXT_CANCELED.to_owned());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("cgroup {} still has processes after cgroup.kill", self.name()));
            }
            tokio::time::sleep(remaining.min(KILL_POLL_INTERVAL)).await;
        }
        Ok(())
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    use super::*;
    use crate::cgroup::cpu_limit::confine_vcpu_threads;

    fn detached(path: &Path) -> Cgroup {
        Cgroup {
            path: path.to_owned(),
            peak: None,
            proc_root: "/proc/".into(),
            vcpu_threads_confined: false,
            removed: false,
        }
    }

    const HELPER_VARIABLE: &str = "CGROUP_VCPU_HELPER";

    fn has_cpu_controller(cgroup: &Cgroup) -> bool {
        std::fs::read_to_string(cgroup.path().join("cgroup.controllers"))
            .is_ok_and(|controllers| controllers.split_whitespace().any(|name| name == "cpu"))
    }

    fn start_vcpu_helper(cgroup: &Cgroup) -> Child {
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cgroup::removal::tests::helper_vcpu_threads", "--nocapture"])
            .env(HELPER_VARIABLE, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = helper.stdout.take().unwrap();
        let named = BufReader::new(output)
            .lines()
            .map_while(Result::ok)
            .any(|line| line == "ready");
        assert!(named, "the helper never named its threads");
        cgroup.add(helper.id()).unwrap();
        helper
    }

    #[test]
    fn helper_vcpu_threads() {
        if std::env::var(HELPER_VARIABLE).as_deref() != Ok("1") {
            return;
        }
        let threads: Vec<_> = ["fc_vcpu 0", "fc_vcpu 1", "fc_api"]
            .into_iter()
            .map(|name| {
                let (named, ready) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    nix::sys::prctl::set_name(&std::ffi::CString::new(name).unwrap()).unwrap();
                    named.send(()).unwrap();
                    std::thread::sleep(Duration::from_secs(3600));
                });
                ready
            })
            .collect();
        threads.iter().for_each(|ready| ready.recv().unwrap());
        println!("ready");
        std::thread::sleep(Duration::from_secs(3600));
    }

    #[test]
    fn the_populated_line_decides() {
        assert_eq!(parse_events_populated("populated 1\nfrozen 0\n"), Ok(true));
        assert_eq!(parse_events_populated("frozen 0\npopulated 0\n"), Ok(false));
        assert_eq!(
            parse_events_populated("populated 2\n"),
            Err("invalid populated value in cgroup.events: \"2\"".to_owned())
        );
        assert_eq!(
            parse_events_populated("frozen 0\n"),
            Err("missing populated value in cgroup.events".to_owned())
        );
    }

    #[tokio::test]
    async fn a_failed_removal_stays_retryable_and_a_removed_one_is_idempotent() {
        let parent = tempfile::tempdir().unwrap().keep();
        let child = parent.join("vcpu");
        std::fs::create_dir(&child).unwrap();
        let blocking_file = child.join("busy");
        std::fs::write(&blocking_file, "").unwrap();
        let mut cgroup = detached(&parent);

        let canceled = CancellationToken::new();
        canceled.cancel();
        assert_eq!(cgroup.remove(&canceled).await, Err(CONTEXT_CANCELED.to_owned()));
        assert!(!cgroup.removed, "failed cleanup must remain retryable");
        assert!(child.exists());

        std::fs::remove_file(&blocking_file).unwrap();
        cgroup.remove(&CancellationToken::new()).await.unwrap();
        assert!(cgroup.removed);
        assert!(!parent.exists());
        cgroup.remove(&CancellationToken::new()).await.unwrap();
    }

    #[tokio::test]
    async fn removing_kills_the_confined_threads_with_their_cgroup() {
        let Some(mut cgroup) = Cgroup::create("test-vcpu-remove-live").ok().filter(has_cpu_controller) else {
            eprintln!("skipping: needs a cgroup v2 with the cpu controller delegated");
            return;
        };
        let mut helper = start_vcpu_helper(&cgroup);
        confine_vcpu_threads(Some(&mut cgroup), 1, 2).unwrap();

        cgroup.remove(&CancellationToken::new()).await.unwrap();

        let exited = tokio::task::spawn_blocking(move || helper.wait().map(|status| status.success()))
            .await
            .unwrap();
        assert_eq!(exited.ok(), Some(false), "the helper outlived the removal");
        assert!(!cgroup.path().exists());
    }
}
