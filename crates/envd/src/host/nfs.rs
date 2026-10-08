use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::future::try_join_all;
use parking_lot::Mutex;

use super::command::combined_output;

const NFS_OPTIONS: &str = "fg,hard,sync,rsize=1048576,wsize=1048576,mountproto=tcp,mountport=2049,proto=tcp,port=2049,nfsvers=3,noacl,noac,lookupcache=none";
const NFS_MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const LAZY_UNMOUNT_TIMEOUT: Duration = Duration::from_secs(2);

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
    pub async fn setup(&self, lifecycle_id: Option<&str>, mounts: &[VolumeMount]) -> Result<(), String> {
        if self.mounting.swap(true, Ordering::SeqCst) {
            tracing::debug!("NFS volumes already mounting");
            return Ok(());
        }
        let lifecycle = lifecycle_id.unwrap_or_default();
        let pending: Vec<&VolumeMount> = mounts
            .iter()
            .filter(|mount| self.needs_mount(mount, lifecycle))
            .collect();
        let mounted = tokio::time::timeout(
            NFS_MOUNT_TIMEOUT,
            try_join_all(pending.into_iter().map(|mount| self.remount(mount, lifecycle))),
        )
        .await
        .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()))
        .map(|_| ());
        self.mounting.store(false, Ordering::SeqCst);
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

    async fn remount(&self, mount: &VolumeMount, lifecycle: &str) -> Result<(), String> {
        tracing::debug!("Setting up {} at {:?}", mount.nfs_target, mount.path);
        self.unmount(&mount.path)
            .await
            .map_err(|e| format!("failed to unmount stale NFS mount at {:?}: {e}", mount.path))?;
        mount_nfs(&mount.nfs_target, &mount.path)
            .await
            .map_err(|e| format!("failed to mount NFS at {:?}: {e}", mount.path))?;
        self.mounted.lock().insert(mount.path.clone(), lifecycle.to_owned());
        Ok(())
    }

    async fn unmount(&self, path: &str) -> Result<(), String> {
        let source = match combined_output("findmnt", &["--noheadings", "--output", "SOURCE", path]).await {
            Ok(source) => source.trim().to_owned(),
            Err(failure) if failure.error == "exit status 1" => return Ok(()),
            Err(failure) => return Err(format!("failed to check if {path:?} is mounted: {}", failure.error)),
        };
        if source.is_empty() {
            return Ok(());
        }
        tracing::debug!("Unmounting stale NFS mount at {path:?} (was: {source})");
        if let Err(forced) = combined_output("umount", &["--force", path]).await {
            tracing::warn!(error = %forced.error, path, output = %forced.output, "Forced NFS umount failed, falling back to lazy");
            let lazy = tokio::time::timeout(LAZY_UNMOUNT_TIMEOUT, combined_output("umount", &["--lazy", path]))
                .await
                .unwrap_or_else(|_| {
                    Err(super::command::CommandFailure {
                        error: "signal: killed".to_owned(),
                        output: String::new(),
                    })
                });
            lazy.map_err(|failure| {
                format!(
                    "failed to unmount stale NFS mount at {path:?}: {}\n{}",
                    failure.error, failure.output
                )
            })?;
        }
        self.mounted.lock().remove(path);
        Ok(())
    }
}

async fn mount_nfs(target: &str, path: &str) -> Result<(), String> {
    let commands: [Vec<&str>; 2] = [
        vec!["mkdir", "-p", path],
        vec!["mount", "-v", "-t", "nfs", "-o", NFS_OPTIONS, target, path],
    ];
    for command in commands {
        combined_output(command[0], &command[1..])
            .await
            .map_err(|failure| format!("`{}` failed: {}\n{}", command.join(" "), failure.error, failure.output))?;
    }
    Ok(())
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
        let failed = mount_nfs("10.0.0.1:/v", "/proc/version/nope").await.unwrap_err();
        assert!(
            failed.starts_with("`mkdir -p /proc/version/nope` failed: exit status 1\n"),
            "{failed}"
        );
    }
}
