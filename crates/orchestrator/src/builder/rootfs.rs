pub mod unpack;

use std::path::Path;

use super::ext4::{BLOCK_SIZE, Ext4};
use super::logs::ScopedLog;
use super::oci::export::{ImageLayer, LayerSource, remote_layers};
use super::oci::pull::{ImageConfig, Puller};
use super::phase::{BuildError, PhaseMeta};
use super::template_files::InjectedFiles;
use super::template_files::layers::{files_layer, symlink_layer};
use crate::go_humanize::bytes;
use crate::proto::{FromImageRegistry, LogLevel};
use unpack::{Mounted, UnpackError, temp_dir, unpack_rootfs};

pub const MB: i64 = 1 << 20;
pub const BASE_ROOTFS_SIZE_LIMIT_MB: i64 = 25_000;
const FILES_LAYER_DIGEST: &str = "files";
const SYMLINK_LAYER_DIGEST: &str = "symlinks";

pub struct RootfsRequest<'a> {
    pub meta: &'a PhaseMeta,
    pub from_image: &'a str,
    pub registry: Option<&'a FromImageRegistry>,
    pub disk_size_mb: i64,
    pub injected: InjectedFiles,
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
    layers
        .extend(injected_layers(&request.injected).map_err(|e| internal(format!("error populating filesystem: {e}")))?);
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
    let files = files_layer(&injected.files()?)?;
    let links = symlink_layer()?;
    Ok([(FILES_LAYER_DIGEST, files), (SYMLINK_LAYER_DIGEST, links)]
        .into_iter()
        .map(|(digest, tar)| ImageLayer {
            digest: digest.to_owned(),
            size: tar.len() as u64,
            source: LayerSource::Bytes(tar),
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
    let temp_root = crate::go_os::parent(&ext4.file);
    let mount_dir = temp_dir(&temp_root, ".ext4-mount-", "error creating temporary mount point")?;
    ext4.mount(mount_dir.path())
        .await
        .map_err(|e| UnpackError::Failed(format!("error mounting ext4 filesystem: {e}")))?;
    let mounted = Mounted::new(mount_dir.path(), "error unmounting ext4 filesystem");
    tracing::debug!(rootfs_path = %ext4.file.display(), tmp_mount = %mount_dir.path().display(), "extracting image to ext4 filesystem");
    let unpacked = unpack_rootfs(log, layers, mount_dir.path(), &temp_root, max_size)
        .await
        .map_err(|e| e.context("error extracting tar to directory"));
    drop(mounted);
    unpacked
}

fn internal(message: String) -> BuildError {
    BuildError::Internal(message)
}
