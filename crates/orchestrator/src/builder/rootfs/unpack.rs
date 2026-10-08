use std::path::Path;

use tempfile::TempDir;
use tokio_util::task::TaskTracker;

use super::teardown::{Step, Teardown};
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

pub fn temp_dir(root: &Path, prefix: &str, context: &str) -> Result<TempDir, UnpackError> {
    tempfile::Builder::new().prefix(prefix).tempdir_in(root).map_err(|e| {
        UnpackError::Failed(format!(
            "{context}: mkdirtemp {}/{prefix}*: {}",
            root.display(),
            crate::go_os::reason(&e)
        ))
    })
}

pub async fn unpack_rootfs(
    log: &ScopedLog<'_>,
    layers: Vec<ImageLayer>,
    destination: &Path,
    temp_root: &Path,
    max_size: i64,
) -> Result<(), UnpackError> {
    let mut teardown = Teardown::default();
    let copied = unpack_into(log, layers, destination, temp_root, max_size, &mut teardown).await;
    teardown.finish().await;
    copied
}

async fn unpack_into(
    log: &ScopedLog<'_>,
    layers: Vec<ImageLayer>,
    destination: &Path,
    temp_root: &Path,
    max_size: i64,
    teardown: &mut Teardown,
) -> Result<(), UnpackError> {
    let oci_dir = temp_dir(
        temp_root,
        ".oci-image-",
        "while creating temporary file for squashed image",
    )?;
    let oci_path = oci_dir.path().to_path_buf();
    teardown.push(Step::Remove {
        dir: oci_dir,
        failure_log: "error removing temporary OCI image",
    });
    let tracker = TaskTracker::new();
    teardown.push(Step::Await(tracker.clone()));
    let layer_paths = create_export(log, layers, &oci_path, &teardown.cancel_token(), &tracker)
        .await
        .map_err(|e| UnpackError::Failed(format!("while creating export of source image: {e}")))?;
    let mount_dir = temp_dir(
        temp_root,
        ".overlayfs-mount-",
        "while creating temporary file for squashed image",
    )?;
    let mount_path = mount_dir.path().to_path_buf();
    teardown.push(Step::Remove {
        dir: mount_dir,
        failure_log: "error removing temporary overlayfs mount point",
    });
    mount_overlay(&layer_paths, &mount_path)
        .map_err(|e| UnpackError::Failed(format!("while mounting overlayfs with layers: {e}")))?;
    teardown.push(Step::Unmount {
        path: mount_path.clone(),
        failure_log: "error unmounting overlayfs mount point",
    });
    log.push(
        LogLevel::Info,
        format!("Root filesystem structure: {}", list_files(&mount_path)?.join(", ")),
    );
    copy_files(&mount_path, destination, max_size)
        .await
        .map_err(|e| match e {
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
