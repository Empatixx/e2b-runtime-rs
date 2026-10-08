use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::builder::logs::ScopedLog;
use crate::builder::oci::copy::{CopyError, ImageTooLarge, copy_files};
use crate::builder::oci::export::{ImageLayer, create_export};
use crate::builder::oci::overlay_mount::mount_overlay;
use crate::proto::LogLevel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpackError {
    TooLarge(ImageTooLarge),
    Failed(String),
}

impl UnpackError {
    pub fn context(self, context: &str) -> Self {
        match self {
            UnpackError::Failed(e) => UnpackError::Failed(format!("{context}: {e}")),
            too_large => too_large,
        }
    }
}

pub struct Mounted {
    path: PathBuf,
    failure_log: &'static str,
}

impl Mounted {
    pub fn new(path: &Path, failure_log: &'static str) -> Self {
        Self {
            path: path.to_path_buf(),
            failure_log,
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let unmounted = std::process::Command::new("umount")
            .arg(&self.path)
            .status()
            .map_err(|e| e.to_string())
            .and_then(|status| match status.success() {
                true => Ok(()),
                false => Err(format!("error unmounting ext4 filesystem: {status}")),
            });
        if let Err(e) = unmounted {
            tracing::error!(error = %e, "{}", self.failure_log);
        }
    }
}

pub fn temp_dir(root: &Path, prefix: &str, context: &str) -> Result<TempDir, UnpackError> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(root)
        .map_err(|e| UnpackError::Failed(format!("{context}: {e}")))
}

pub async fn unpack_rootfs(
    log: &ScopedLog<'_>,
    layers: Vec<ImageLayer>,
    destination: &Path,
    temp_root: &Path,
    max_size: i64,
) -> Result<(), UnpackError> {
    let oci_dir = temp_dir(
        temp_root,
        ".oci-image-",
        "while creating temporary file for squashed image",
    )?;
    let layer_paths = create_export(log, layers, oci_dir.path())
        .await
        .map_err(|e| UnpackError::Failed(format!("while creating export of source image: {e}")))?;
    let mount_dir = temp_dir(
        temp_root,
        ".overlayfs-mount-",
        "while creating temporary file for squashed image",
    )?;
    mount_overlay(&layer_paths, mount_dir.path())
        .map_err(|e| UnpackError::Failed(format!("while mounting overlayfs with layers: {e}")))?;
    let overlay = Mounted::new(mount_dir.path(), "error unmounting overlayfs mount point");
    log.push(
        LogLevel::Info,
        format!(
            "Root filesystem structure: {}",
            list_files(mount_dir.path())?.join(", ")
        ),
    );
    let copied = copy_files(mount_dir.path(), destination, max_size).await;
    drop(overlay);
    drop(mount_dir);
    drop(oci_dir);
    copied.map_err(|e| match e {
        CopyError::TooLarge(too_large) => UnpackError::TooLarge(too_large),
        CopyError::Failed(e) => UnpackError::Failed(format!(
            "while copying files from overlayfs to destination directory: {e}"
        )),
    })
}

fn list_files(dir: &Path) -> Result<Vec<String>, UnpackError> {
    let mut names = std::fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|e| {
            UnpackError::Failed(format!(
                "while listing files in overlayfs: while reading directory {}: {e}",
                dir.display()
            ))
        })?;
    names.sort();
    Ok(names)
}
