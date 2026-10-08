pub mod grow;
mod measure;
mod offline;
mod resize;
pub mod sizing;

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use uuid::Uuid;

use self::grow::{FreeSpace, GrownRootfs, RootfsDiff, RootfsGrower};
use super::steps::StepsBuild;
use crate::builder::hash::ensure_free_disk_hash;
use crate::builder::index::{LayerIndex, MetadataError};
use crate::builder::layer::BUILD_CACHE_ORIGIN;
use crate::builder::logs::ScopedLog;
use crate::builder::phase::{BuildError, LayerResult, Phase, PhaseMeta};
use crate::proto::LogLevel;
use crate::sandbox::snapshot::filesystem::FilesystemSnapshot;
use crate::storage::build::Build;
use crate::template::metadata::{CURRENT_VERSION, TemplateMetadata, TemplateVersions};

pub const RESIZE_DISK: &str = "resize-disk";
pub const RESIZE_DISK_TEXT: &str = "Resizing disk";
pub const RESIZE_DISK_TIMEOUT: Duration = Duration::from_secs(3600);
const FILESYSTEM_ONLY_VERSION: u64 = CURRENT_VERSION;
const MB_SHIFT: u32 = 20;

const METADATA_FILE: &str = "metadata.json";
const DEADLINE_EXCEEDED: &str = "context deadline exceeded";

pub struct EnsureFreeDiskPhase {
    pub build: Arc<StepsBuild>,
    pub force: bool,
    pub free_disk_size_mb: i64,
}

impl EnsureFreeDiskPhase {
    async fn cached_layer(&self, source: &LayerResult, hash: String) -> LayerResult {
        let config = &self.build.env.config;
        LayerLookup {
            index: &self.build.index,
            force: self.force,
            kernel_version: &config.kernel_version,
            firecracker_version: &config.firecracker_version,
        }
        .resolve(source, hash)
        .await
    }

    async fn resize_within_deadline(
        &self,
        log: &ScopedLog<'_>,
        source: &LayerResult,
        current: LayerResult,
    ) -> Result<LayerResult, BuildError> {
        tokio::time::timeout(RESIZE_DISK_TIMEOUT, self.resize_disk(log, source, current))
            .await
            .unwrap_or_else(|_| Err(BuildError::Internal(format!("resize disk: {DEADLINE_EXCEEDED}"))))
    }

    async fn resize_disk(
        &self,
        log: &ScopedLog<'_>,
        source: &LayerResult,
        current: LayerResult,
    ) -> Result<LayerResult, BuildError> {
        let node = &self.build.env.node;
        let source_id = Uuid::parse_str(&source.metadata.template.build_id)
            .map_err(|e| BuildError::Internal(format!("get source template: failed to parse build id: {e}")))?;
        let template = node.templates.get(source_id);
        let source_build = template
            .build()
            .await
            .map_err(|e| BuildError::Internal(format!("get source rootfs: {e}")))?;
        let layer_id = Uuid::parse_str(&current.metadata.template.build_id)
            .map_err(|e| BuildError::Internal(format!("parse resize-disk layer build id: {e}")))?;
        let dir = node.layer_dir(layer_id);
        let grower = RootfsGrower {
            source: node.rootfs_file(source_build.rootfs),
            layer_id,
            scratch_dir: node.scratch_dir().to_owned(),
            dir: dir.clone(),
        };
        let layer = self
            .grow_and_persist(log, &grower, &current, &dir)
            .await
            .inspect_err(|_| drop(std::fs::remove_dir_all(&dir)))?;
        drop(template);
        Ok(LayerResult {
            metadata: layer,
            cached: source.cached,
            hash: current.hash,
        })
    }

    async fn grow_and_persist(
        &self,
        log: &ScopedLog<'_>,
        grower: &RootfsGrower,
        current: &LayerResult,
        dir: &std::path::Path,
    ) -> Result<TemplateMetadata, BuildError> {
        let grown = grower
            .grow_and_export(self.free_disk_size_mb)
            .await
            .map_err(|e| BuildError::Internal(format!("resize disk: {e}")))?;
        log_outcome(log, &grown.free);
        let metadata = mark_filesystem_only(current.metadata.clone());
        let layer = write_layer_files(grower.layer_id, grown, &metadata, dir)?;
        self.build
            .executor(log)
            .persist_filesystem_layer(layer, dir.to_owned(), &metadata, &current.hash, BUILD_CACHE_ORIGIN)
            .map_err(|e| BuildError::Internal(format!("persist resized rootfs layer: {e}")))?;
        Ok(metadata)
    }
}

struct LayerLookup<'a> {
    index: &'a LayerIndex,
    force: bool,
    kernel_version: &'a str,
    firecracker_version: &'a str,
}

impl LayerLookup<'_> {
    async fn resolve(&self, source: &LayerResult, hash: String) -> LayerResult {
        if !source.cached || self.force {
            return self.fresh(source, hash);
        }
        match self.indexed_metadata(&hash).await {
            Some(metadata) => LayerResult {
                metadata,
                cached: true,
                hash,
            },
            None => self.fresh(source, hash),
        }
    }

    fn fresh(&self, source: &LayerResult, hash: String) -> LayerResult {
        LayerResult {
            metadata: fresh_layer_metadata(&source.metadata, self.kernel_version, self.firecracker_version),
            cached: false,
            hash,
        }
    }

    async fn indexed_metadata(&self, hash: &str) -> Option<TemplateMetadata> {
        let build_id = self
            .index
            .layer_build_id(hash)
            .await
            .inspect_err(|e| tracing::info!(error = %e, hash, "resize-disk layer unavailable in cache, building it"))
            .ok()?;
        self.index
            .metadata_of(&build_id)
            .await
            .inspect_err(|e| {
                let (MetadataError::Missing(error) | MetadataError::Failed(error)) = e;
                tracing::info!(error = %error, hash, build_id = %build_id, "resize-disk artifact unavailable in cache, building it");
            })
            .ok()
    }
}

fn log_outcome(log: &ScopedLog<'_>, free: &FreeSpace) {
    if free.before >= free.target {
        return log.push(LogLevel::Info, no_resize_needed(free.before, free.target));
    }
    if free.after < free.target {
        tracing::warn!(
            requested_free_bytes = free.target,
            free_before_bytes = free.before,
            free_after_bytes = free.after,
            "resize-disk target undershot after ext4 resize"
        );
    }
    log.push(LogLevel::Info, resized(free.before, free.after));
}

fn write_layer_files(
    layer_id: Uuid,
    grown: GrownRootfs,
    metadata: &TemplateMetadata,
    dir: &std::path::Path,
) -> Result<FilesystemSnapshot, BuildError> {
    let metadata_file = dir.join(METADATA_FILE);
    std::fs::create_dir_all(dir).map_err(|e| BuildError::Internal(format!("create cache paths: {e}")))?;
    std::fs::write(&metadata_file, metadata.to_json())
        .map_err(|e| BuildError::Internal(format!("write resize-disk layer metadata: {e}")))?;
    let (rootfs_diff, rootfs_bytes) = match grown.diff {
        RootfsDiff::Unchanged => (None, 0),
        RootfsDiff::File { path, bytes } => (Some(path), bytes),
    };
    Ok(FilesystemSnapshot {
        build: Build::filesystem_only(layer_id, grown.header),
        rootfs_diff,
        rootfs_bytes,
        metadata_file,
    })
}

impl Phase for EnsureFreeDiskPhase {
    fn prefix(&self) -> String {
        RESIZE_DISK.to_owned()
    }

    fn describe(&self) -> String {
        RESIZE_DISK_TEXT.to_owned()
    }

    fn meta(&self) -> PhaseMeta {
        phase_meta()
    }

    fn hash<'a>(&'a self, source: &'a LayerResult) -> BoxFuture<'a, Result<String, BuildError>> {
        let hash = ensure_free_disk_hash(&source.hash, self.free_disk_size_mb);
        async move { Ok(hash) }.boxed()
    }

    fn layer<'a>(&'a self, source: &'a LayerResult, hash: String) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move { Ok(self.cached_layer(source, hash).await) }.boxed()
    }

    fn build<'a>(
        &'a self,
        log: ScopedLog<'a>,
        source: &'a LayerResult,
        current: LayerResult,
    ) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move { self.resize_within_deadline(&log, source, current).await }.boxed()
    }
}

pub fn phase_meta() -> PhaseMeta {
    PhaseMeta {
        phase: RESIZE_DISK,
        step_type: RESIZE_DISK.to_owned(),
        step_number: None,
    }
}

pub fn free_disk_size_mb(free_disk_size_mb: Option<i32>, disk_size_mb: i32) -> i64 {
    free_disk_size_mb.unwrap_or(disk_size_mb).into()
}

pub fn fresh_layer_metadata(
    source: &TemplateMetadata,
    kernel_version: &str,
    firecracker_version: &str,
) -> TemplateMetadata {
    TemplateMetadata {
        template: TemplateVersions {
            build_id: Uuid::new_v4().to_string(),
            kernel_version: kernel_version.to_owned(),
            firecracker_version: firecracker_version.to_owned(),
        },
        ..source.clone()
    }
}

pub fn mark_filesystem_only(metadata: TemplateMetadata) -> TemplateMetadata {
    TemplateMetadata {
        filesystem_only: true,
        version: metadata.version.max(FILESYSTEM_ONLY_VERSION),
        ..metadata
    }
}

pub fn no_resize_needed(free_before: i64, target: i64) -> String {
    format!(
        "resize-disk: {} MiB free already >= target {} MiB, no resize needed",
        free_before >> MB_SHIFT,
        target >> MB_SHIFT
    )
}

pub fn resized(free_before: i64, free_after: i64) -> String {
    format!(
        "resize-disk: free {} -> {} MiB",
        free_before >> MB_SHIFT,
        free_after >> MB_SHIFT
    )
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::storage::backend::Storage;

    #[test]
    fn the_phase_reads_like_upstream() {
        let meta = phase_meta();

        assert_eq!(meta.step(), "resize-disk");
        assert_eq!(meta.phase, "resize-disk");
        assert_eq!(meta.log_fields().get("step_number").map(String::as_str), Some("<nil>"));
        assert_eq!(RESIZE_DISK_TEXT, "Resizing disk");
    }

    #[test]
    fn the_free_disk_size_falls_back_to_the_disk_size() {
        assert_eq!(free_disk_size_mb(Some(512), 2048), 512);
        assert_eq!(free_disk_size_mb(Some(0), 2048), 0);
        assert_eq!(free_disk_size_mb(None, 2048), 2048);
    }

    #[test]
    fn a_fresh_layer_keeps_the_source_and_gets_a_new_build_id() {
        let source = TemplateMetadata {
            template: TemplateVersions {
                build_id: "old".to_owned(),
                kernel_version: "k0".to_owned(),
                firecracker_version: "f0".to_owned(),
            },
            from_image: Some("debian".to_owned()),
            ..TemplateMetadata::default()
        };

        let fresh = fresh_layer_metadata(&source, "k1", "f1");

        assert!(Uuid::parse_str(&fresh.template.build_id).is_ok());
        assert_eq!(fresh.template.kernel_version, "k1");
        assert_eq!(fresh.template.firecracker_version, "f1");
        assert_eq!(fresh.from_image.as_deref(), Some("debian"));
        assert_eq!(source.template.build_id, "old");
    }

    #[test]
    fn marking_filesystem_only_raises_the_version_to_at_least_two() {
        let old = TemplateMetadata {
            version: 1,
            ..TemplateMetadata::default()
        };
        let newer = TemplateMetadata {
            version: 3,
            ..TemplateMetadata::default()
        };

        assert_eq!(
            (
                mark_filesystem_only(old.clone()).filesystem_only,
                mark_filesystem_only(old).version
            ),
            (true, 2)
        );
        assert_eq!(mark_filesystem_only(newer).version, 3);
    }

    #[test]
    fn the_user_log_lines_are_whole_mebibytes() {
        assert_eq!(
            no_resize_needed(600 << 20, 512 << 20),
            "resize-disk: 600 MiB free already >= target 512 MiB, no resize needed"
        );
        assert_eq!(
            no_resize_needed((512 << 20) + 5, 512 << 20),
            "resize-disk: 512 MiB free already >= target 512 MiB, no resize needed"
        );
        assert_eq!(resized(-(3 << 20), 700 << 20), "resize-disk: free -3 -> 700 MiB");
    }

    fn stored_layer(version: u64) -> TemplateMetadata {
        TemplateMetadata {
            version,
            filesystem_only: true,
            template: TemplateVersions {
                build_id: "ensure-build".to_owned(),
                kernel_version: "k".to_owned(),
                firecracker_version: "f".to_owned(),
            },
            ..TemplateMetadata::default()
        }
    }

    fn cached_source() -> LayerResult {
        LayerResult {
            metadata: TemplateMetadata {
                from_image: Some("debian".to_owned()),
                ..TemplateMetadata::default()
            },
            cached: true,
            hash: "source".to_owned(),
        }
    }

    async fn resolve(storage: &Storage, source: &LayerResult, force: bool) -> LayerResult {
        let index = LayerIndex::new(storage.clone(), storage.clone(), "scope");
        LayerLookup {
            index: &index,
            force,
            kernel_version: "kernel",
            firecracker_version: "firecracker",
        }
        .resolve(source, "hash".to_owned())
        .await
    }

    async fn store_layer(storage: &Storage, version: u64) {
        storage
            .put(
                "ensure-build/metadata.json",
                Bytes::from(stored_layer(version).to_json()),
            )
            .await
            .unwrap();
        LayerIndex::new(storage.clone(), storage.clone(), "scope")
            .save("hash", "ensure-build")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_cached_source_reconnects_to_the_indexed_layer() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        store_layer(&storage, 2).await;

        let layer = resolve(&storage, &cached_source(), false).await;

        assert!(layer.cached);
        assert_eq!(layer.metadata, stored_layer(2));
        assert_eq!(layer.hash, "hash");
    }

    #[tokio::test]
    async fn a_fresh_or_forced_source_never_reconnects_to_an_older_layer() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        store_layer(&storage, 2).await;
        let fresh_source = LayerResult {
            cached: false,
            ..cached_source()
        };

        let forced = resolve(&storage, &cached_source(), true).await;
        let rebuilt = resolve(&storage, &fresh_source, false).await;

        for layer in [forced, rebuilt] {
            assert!(!layer.cached);
            assert_eq!(layer.hash, "hash");
            assert_eq!(layer.metadata.from_image.as_deref(), Some("debian"));
            assert_eq!(layer.metadata.template.kernel_version, "kernel");
            assert_eq!(layer.metadata.template.firecracker_version, "firecracker");
            assert!(Uuid::parse_str(&layer.metadata.template.build_id).is_ok());
        }
    }

    #[tokio::test]
    async fn a_missing_index_entry_or_artifact_builds_a_fresh_layer() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        let unindexed = resolve(&storage, &cached_source(), false).await;
        LayerIndex::new(storage.clone(), storage.clone(), "scope")
            .save("hash", "ensure-build")
            .await
            .unwrap();
        let without_artifact = resolve(&storage, &cached_source(), false).await;
        store_layer(&storage, 1).await;
        let outdated = resolve(&storage, &cached_source(), false).await;

        assert!(
            [unindexed, without_artifact, outdated]
                .iter()
                .all(|layer| !layer.cached)
        );
    }
}
