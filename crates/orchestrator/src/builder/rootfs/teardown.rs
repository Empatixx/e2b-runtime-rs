use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const UNMOUNT_ATTEMPTS: u32 = 50;
const UNMOUNT_RETRY: Duration = Duration::from_millis(100);

pub enum Step {
    Unmount { path: PathBuf, failure_log: &'static str },
    Remove { dir: TempDir, failure_log: &'static str },
    Await(TaskTracker),
}

#[derive(Default)]
pub struct Teardown {
    steps: Vec<Step>,
    cancel: CancellationToken,
}

impl Teardown {
    pub fn push(&mut self, step: Step) {
        self.steps.push(step);
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub async fn finish(mut self) {
        let steps = std::mem::take(&mut self.steps);
        run(steps, false).await;
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        let steps = std::mem::take(&mut self.steps);
        if steps.is_empty() {
            return;
        }
        self.cancel.cancel();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn(run(steps, true))),
            Err(_) => steps.into_iter().rev().for_each(|step| run_blocking(step, true)),
        }
    }
}

async fn run(steps: Vec<Step>, retry_busy: bool) {
    for step in steps.into_iter().rev() {
        match step {
            Step::Await(tracker) => {
                tracker.close();
                tracker.wait().await;
            }
            step => {
                let _ = tokio::task::spawn_blocking(move || run_blocking(step, retry_busy)).await;
            }
        }
    }
}

fn run_blocking(step: Step, retry_busy: bool) {
    match step {
        Step::Unmount { path, failure_log } => {
            if let Err(e) = unmount(&path, retry_busy) {
                tracing::error!(error = %e, "{failure_log}");
            }
        }
        Step::Remove { dir, failure_log } => {
            let path = dir.path().to_path_buf();
            if let Err(e) = dir.close() {
                tracing::error!(error = %e, path = %path.display(), "{failure_log}");
            }
        }
        Step::Await(_) => {}
    }
}

fn unmount(path: &std::path::Path, retry_busy: bool) -> Result<(), String> {
    let attempts = match retry_busy {
        true => UNMOUNT_ATTEMPTS,
        false => 1,
    };
    let mut last = String::new();
    for attempt in 1..=attempts {
        match std::process::Command::new("umount").arg(path).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                last = format!(
                    "error unmounting ext4 filesystem: exit status {}",
                    status.code().unwrap_or(-1)
                )
            }
            Err(e) => last = format!("error unmounting ext4 filesystem: {e}"),
        }
        if attempt < attempts {
            std::thread::sleep(UNMOUNT_RETRY);
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dropped_teardown_cancels_waits_for_tasks_then_removes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let tracker = TaskTracker::new();
        let mut teardown = Teardown::default();
        let cancel = teardown.cancel_token();
        teardown.push(Step::Remove {
            dir,
            failure_log: "error removing temporary OCI image",
        });
        teardown.push(Step::Await(tracker.clone()));
        let written = path.join("written");
        tracker.spawn(async move {
            cancel.cancelled().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::fs::write(&written, b"late").unwrap();
        });

        drop(teardown);

        tokio::time::timeout(Duration::from_secs(5), async {
            while path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(tracker.is_closed());
    }
}
