pub mod teardown;
pub mod unpack;

use std::path::Path;

use super::ext4::{BLOCK_SIZE, Ext4};
use super::logs::ScopedLog;
use super::oci::export::{ImageLayer, LayerSource, remote_layers};
use super::oci::pull::{ImageConfig, Puller};
use super::oci::registry::digest_of;
use super::phase::{BuildError, PhaseMeta};
use super::template_files::layers::{files_layer, gzipped, symlink_layer};
use super::template_files::{InjectedFiles, InjectedSources};
use crate::go_humanize::bytes;
use crate::proto::{FromImageRegistry, LogLevel};
use teardown::{Step, Teardown};
use unpack::{UnpackError, temp_dir, unpack_rootfs};

pub const MB: i64 = 1 << 20;
pub const BASE_ROOTFS_SIZE_LIMIT_MB: i64 = 25_000;

pub struct RootfsRequest<'a> {
    pub meta: &'a PhaseMeta,
    pub from_image: &'a str,
    pub registry: Option<&'a FromImageRegistry>,
    pub disk_size_mb: i64,
    pub injected: InjectedSources,
    pub path: &'a Path,
}

pub async fn create_ext4_filesystem(
    log: &ScopedLog<'_>,
    puller: &Puller,
    request: RootfsRequest<'_>,
) -> Result<ImageConfig, BuildError> {
    log.push(LogLevel::Debug, "Requesting Docker Image");
    let image = puller
        .get_public_image(request.from_image, request.registry)
        .await
        .map_err(|e| BuildError::user(request.meta, e))?;
    log.push(
        LogLevel::Info,
        format!("Base Docker image size: {}", bytes(image.size())),
    );
    log.push(LogLevel::Debug, "Setting up system files");
    let mut layers = remote_layers(&image);
    let injected = request
        .injected
        .read()
        .and_then(|files| injected_layers(&files))
        .map_err(|e| internal(format!("error populating filesystem: {e}")))?;
    layers.extend(injected);
    log.push(LogLevel::Info, "Creating file system and pulling Docker image");
    let ext4 = Ext4::new(request.path);
    let ext4_size = to_ext4(log, &ext4, layers, BASE_ROOTFS_SIZE_LIMIT_MB * MB)
        .await
        .map_err(|e| match e {
            UnpackError::TooLarge(too_large) => BuildError::user(request.meta, too_large.to_string()),
            UnpackError::Failed(e) => internal(format!("error converting oci to ext4: {e}")),
        })?;
    log.push(LogLevel::Debug, "Filesystem cleanup");
    ext4.make_writable()
        .await
        .map_err(|e| internal(format!("error making rootfs file writable: {e}")))?;
    let free = ext4
        .free_space(BLOCK_SIZE)
        .await
        .map_err(|e| internal(format!("error getting free space: {e}")))?;
    let disk_add = request.disk_size_mb * MB - free;
    tracing::debug!(
        size_current = ext4_size,
        size_add = disk_add,
        size_free = free,
        "adding disk size diff to rootfs"
    );
    if disk_add > 0 {
        ext4.enlarge(disk_add)
            .await
            .map_err(|e| internal(format!("error enlarging rootfs: {e}")))?;
    }
    let check = ext4.check_integrity(true).await;
    tracing::debug!(result = ?check, "filesystem ext4 integrity");
    check.map_err(|e| internal(format!("error checking ext4 filesystem integrity: {e}")))?;
    Ok(image.config)
}

fn injected_layers(injected: &InjectedFiles) -> Result<Vec<ImageLayer>, String> {
    let tars = [files_layer(&injected.files()?)?, symlink_layer()?];
    Ok(tars
        .iter()
        .map(|tar| gzipped(tar))
        .map(|layer| ImageLayer {
            digest: digest_of(&layer),
            size: layer.len() as u64,
            source: LayerSource::Bytes(layer),
        })
        .collect())
}

async fn to_ext4(log: &ScopedLog<'_>, ext4: &Ext4, layers: Vec<ImageLayer>, max_size: i64) -> Result<i64, UnpackError> {
    ext4.make(max_size / MB, BLOCK_SIZE)
        .await
        .map_err(|e| UnpackError::Failed(format!("error creating ext4 file: {e}")))?;
    extract_to_ext4(log, ext4, layers, max_size)
        .await
        .map_err(|e| e.context("error extracting image to ext4 filesystem"))?;
    ext4.check_integrity(true)
        .await
        .map_err(|e| UnpackError::Failed(format!("error checking filesystem integrity after ext4 creation: {e}")))?;
    let size = ext4
        .shrink()
        .await
        .map_err(|e| UnpackError::Failed(format!("error shrinking ext4 filesystem: {e}")))?;
    ext4.check_integrity(true)
        .await
        .map_err(|e| UnpackError::Failed(format!("error checking filesystem integrity after shrinking: {e}")))?;
    Ok(size)
}

async fn extract_to_ext4(
    log: &ScopedLog<'_>,
    ext4: &Ext4,
    layers: Vec<ImageLayer>,
    max_size: i64,
) -> Result<(), UnpackError> {
    let mut teardown = Teardown::default();
    let extracted = extract_into(log, ext4, layers, max_size, &mut teardown).await;
    teardown.finish().await;
    extracted
}

async fn extract_into(
    log: &ScopedLog<'_>,
    ext4: &Ext4,
    layers: Vec<ImageLayer>,
    max_size: i64,
    teardown: &mut Teardown,
) -> Result<(), UnpackError> {
    let temp_root = crate::go_os::parent(&ext4.file);
    let mount_dir = temp_dir(&temp_root, ".ext4-mount-", "error creating temporary mount point")?;
    let mount_path = mount_dir.path().to_path_buf();
    teardown.push(Step::Remove {
        dir: mount_dir,
        failure_log: "error removing temporary mount point",
    });
    ext4.mount(&mount_path)
        .await
        .map_err(|e| UnpackError::Failed(format!("error mounting ext4 filesystem: {e}")))?;
    teardown.push(Step::Unmount {
        path: mount_path.clone(),
        failure_log: "error unmounting ext4 filesystem",
    });
    tracing::debug!(rootfs_path = %ext4.file.display(), tmp_mount = %mount_path.display(), "extracting image to ext4 filesystem");
    unpack_rootfs(log, layers, &mount_path, &temp_root, max_size)
        .await
        .map_err(|e| e.context("error extracting tar to directory"))
}

fn internal(message: String) -> BuildError {
    BuildError::Internal(message)
}
