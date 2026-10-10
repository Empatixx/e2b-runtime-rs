use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

use futures::future::try_join_all;
use parking_lot::Mutex;

use super::command::{CommandFailure, group_output};
use crate::util::paths::look_path;

const NFS_OPTIONS: &str = "fg,hard,sync,rsize=1048576,wsize=1048576,mountproto=tcp,mountport=2049,proto=tcp,port=2049,nfsvers=3,noacl,noac,lookupcache=none,retry=0";
const NFS_MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const LAZY_UNMOUNT_TIMEOUT: Duration = Duration::from_secs(2);
const NFS_MOUNT_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeMount {
    pub nfs_target: String,
    pub path: String,
}

#[derive(Default)]
pub struct NfsMounts {
    mounting: AtomicBool,
    mounted: Mutex<HashMap<String, String>>,
}

impl NfsMounts {
    pub fn export(&self) -> Vec<(String, String)> {
        let mut mounts: Vec<(String, String)> = self
            .mounted
            .lock()
            .iter()
            .map(|(path, lifecycle)| (path.clone(), lifecycle.clone()))
            .collect();
        mounts.sort();
        mounts
    }

    pub fn import(&self, mounts: impl IntoIterator<Item = (String, String)>) {
        self.mounted.lock().extend(mounts);
    }

    pub async fn setup(&self, lifecycle_id: Option<&str>, mounts: &[VolumeMount]) -> Result<(), String> {
        if self.mounting.swap(true, Ordering::SeqCst) {
            tracing::debug!("NFS volumes already mounting");
            return Ok(());
        }
        tracing::debug!("Setting up NFS volumes");
        let started = Instant::now();
        let deadline = started + NFS_MOUNT_TIMEOUT;
        let lifecycle = lifecycle_id.unwrap_or_default();
        let pending: Vec<&VolumeMount> = mounts
            .iter()
            .filter(|mount| self.needs_mount(mount, lifecycle))
            .collect();
        let mounted = try_join_all(
            pending
                .into_iter()
                .map(|mount| self.remount(mount, lifecycle, deadline)),
        )
        .await
        .map(|_| ());
        self.mounting.store(false, Ordering::SeqCst);
        if mounted.is_ok() {
            tracing::info!(
                volumes = mounts.len(),
                duration_ms = milliseconds(started.elapsed()),
                "NFS volumes set up"
            );
        }
        mounted
    }

    fn needs_mount(&self, mount: &VolumeMount, lifecycle: &str) -> bool {
        let mounted = self.mounted.lock().get(&mount.path).cloned();
        match mounted {
            Some(current) if current == lifecycle => {
                tracing::debug!("Skipping {:?}, already mounted for lifecycle {lifecycle:?}", mount.path);
                false
            }
            Some(current) => {
                tracing::debug!("Lifecycle changed for {:?}: {current:?} -> {lifecycle:?}", mount.path);
                true
            }
            None => true,
        }
    }

    async fn remount(&self, mount: &VolumeMount, lifecycle: &str, deadline: Instant) -> Result<(), String> {
        tracing::debug!("Setting up {} at {:?}", mount.nfs_target, mount.path);
        self.unmount(&mount.path, deadline)
            .await
            .map_err(|e| format!("failed to unmount stale NFS mount at {:?}: {e}", mount.path))?;
        mount_nfs(&mount.nfs_target, &mount.path, deadline)
            .await
            .map_err(|e| format!("failed to mount NFS at {:?}: {e}", mount.path))?;
        self.mounted.lock().insert(mount.path.clone(), lifecycle.to_owned());
        Ok(())
    }

    async fn unmount(&self, path: &str, deadline: Instant) -> Result<(), String> {
        let source = match run_nfs_command(deadline, "findmnt", &["--noheadings", "--output", "SOURCE", path]).await {
            Ok(source) => source.trim().to_owned(),
            Err(failure) if failure.error == "exit status 1" => return Ok(()),
            Err(failure) => return Err(format!("failed to check if {path:?} is mounted: {}", failure.error)),
        };
        if source.is_empty() {
            return Ok(());
        }
        tracing::debug!("Unmounting stale NFS mount at {path:?} (was: {source})");
        let started = Instant::now();
        let mut mode = "force";
        if let Err(forced) = run_nfs_command(deadline, "umount", &["--force", path]).await {
            tracing::warn!(error = %forced.error, path, output = %forced.output, "Forced NFS umount failed, falling back to lazy");
            mode = "lazy";
            run_nfs_command(Instant::now() + LAZY_UNMOUNT_TIMEOUT, "umount", &["--lazy", path])
                .await
                .map_err(|failure| {
                    format!(
                        "failed to unmount stale NFS mount at {path:?}: {}\n{}",
                        failure.error, failure.output
                    )
                })?;
        }
        self.mounted.lock().remove(path);
        tracing::info!(
            path,
            source,
            mode,
            duration_ms = milliseconds(started.elapsed()),
            "Unmounted stale NFS mount"
        );
        Ok(())
    }
}

async fn run_nfs_command(deadline: Instant, program: &str, args: &[&str]) -> Result<String, CommandFailure> {
    let line = command_line(program, args);
    tracing::info!(cmd = %line, "Running NFS command");
    let started = Instant::now();
    let result = group_output(deadline, program, args).await;
    tracing::info!(
        cmd = %line,
        error = result.as_ref().err().map(|failure| failure.error.as_str()),
        duration_ms = milliseconds(started.elapsed()),
        "NFS command finished"
    );
    result
}

fn command_line(program: &str, args: &[&str]) -> String {
    let resolved = look_path(program).map_or_else(|| program.to_owned(), |path| path.display().to_string());
    std::iter::once(resolved.as_str())
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_nanos() as f64 / 1e6
}

async fn mount_nfs(target: &str, path: &str, deadline: Instant) -> Result<(), String> {
    run_nfs_command(deadline, "mkdir", &["-p", path])
        .await
        .map_err(|failure| format!("`mkdir -p {path}` failed: {}\n{}", failure.error, failure.output))?;
    let mount = ["-v", "-t", "nfs", "-o", NFS_OPTIONS, target, path];
    let started = Instant::now();
    let (attempts, mounted) = retry_until_done(deadline, NFS_MOUNT_RETRY_DELAY, |attempt| async move {
        run_nfs_command(deadline, "mount", &mount).await.map(|_| ()).map_err(|failure| {
            tracing::warn!(error = %failure.error, target, path, attempt, output = %failure.output, "NFS mount attempt failed");
            format!(
                "`mount {}` failed: {}\n{}",
                mount.join(" "),
                failure.error,
                failure.output
            )
        })
    })
    .await;
    mounted.map_err(|e| format!("after {attempts} attempts: {e}"))?;
    tracing::info!(
        target,
        path,
        attempts,
        duration_ms = milliseconds(started.elapsed()),
        "Mounted NFS volume"
    );
    Ok(())
}

async fn retry_until_done<F, Fut>(deadline: Instant, delay: Duration, mut attempt: F) -> (usize, Result<(), String>)
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut attempts = 0;
    loop {
        attempts += 1;
        let failed = match attempt(attempts).await {
            Ok(()) => return (attempts, Ok(())),
            Err(failed) => failed,
        };
        if timeout_at(deadline, tokio::time::sleep(delay)).await.is_err() {
            return (attempts, Err(failed));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_remounted_only_for_a_new_lifecycle() {
        let mounts = NfsMounts::default();
        let mount = VolumeMount {
            nfs_target: "10.0.0.1:/v".to_owned(),
            path: "/mnt/v".to_owned(),
        };
        assert!(mounts.needs_mount(&mount, "l1"));
        mounts.mounted.lock().insert("/mnt/v".to_owned(), "l1".to_owned());
        assert!(!mounts.needs_mount(&mount, "l1"));
        assert!(mounts.needs_mount(&mount, "l2"));
    }

    #[tokio::test]
    async fn a_failing_mount_reports_the_command_and_its_output() {
        let failed = mount_nfs("10.0.0.1:/v", "/proc/version/nope", Instant::now() + NFS_MOUNT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(
            failed.starts_with("`mkdir -p /proc/version/nope` failed: exit status 1\n"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn the_mount_options_make_a_single_attempt() {
        assert!(NFS_OPTIONS.ends_with(",retry=0"));
        assert!(NFS_OPTIONS.starts_with("fg,hard,sync,"), "{NFS_OPTIONS}");
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_succeeds() {
        let (attempts, result) = retry_until_done(
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(1),
            |attempt| async move {
                match attempt < 3 {
                    true => Err("connection refused".to_owned()),
                    false => Ok(()),
                }
            },
        )
        .await;

        assert_eq!((attempts, result), (3, Ok(())));
    }

    #[tokio::test]
    async fn the_last_attempts_error_is_returned_at_the_deadline() {
        let (attempts, result) = retry_until_done(
            Instant::now() + Duration::from_millis(50),
            Duration::from_millis(5),
            |attempt| async move { Err(format!("connection refused {attempt}")) },
        )
        .await;

        assert!(attempts > 1);
        assert_eq!(result, Err(format!("connection refused {attempts}")));
    }

    #[test]
    fn the_logged_command_starts_with_the_resolved_program() {
        let line = command_line("sh", &["-c", "true"]);

        assert!(line.starts_with('/') && line.ends_with("/sh -c true"), "{line}");
        assert_eq!(
            command_line("no-such-program-anywhere", &["x"]),
            "no-such-program-anywhere x"
        );
    }
}
