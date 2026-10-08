use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::RESIZE_DISK;
use super::deadline::Deadline;
use super::measure::measure_free;
use super::resize::resize_offline;
use super::sizing::{compute_grown_size, validate_source_geometry};
use crate::block::build_file::BuildFile;
use crate::block::overlay::Overlay;
use crate::storage::header::mapping::validate;
use crate::storage::header::{DiffMetadata, Header, PAGE_SIZE};

const ROOTFS_DIFF: &str = "rootfs.diff";
const MB_SHIFT: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsDiff {
    Unchanged,
    File { path: PathBuf, bytes: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeSpace {
    pub before: i64,
    pub after: i64,
    pub target: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrownRootfs {
    pub diff: RootfsDiff,
    pub header: Header,
    pub free: FreeSpace,
}

pub struct RootfsGrower {
    pub source: Arc<BuildFile>,
    pub layer_id: Uuid,
    pub scratch_dir: PathBuf,
    pub dir: PathBuf,
    pub deadline: Deadline,
}

impl RootfsGrower {
    pub async fn grow_and_export(&self, target_free_mb: i64) -> Result<GrownRootfs, String> {
        let target = target_bytes(target_free_mb)?;
        let header = self.source.header();
        let source_size = header.metadata.size as i64;
        let block_size = header.metadata.block_size as i64;
        validate_source_geometry(
            source_size,
            block_size,
            header.metadata.size,
            header.metadata.block_size,
        )?;
        let before = measure_free(&self.source, &self.cache_path("measure"), block_size, self.deadline).await?;
        match before >= target {
            true => self.unchanged(before, target),
            false => {
                let new_size = compute_grown_size(source_size, target, before)?;
                self.resize_and_export(source_size, new_size, before, target).await
            }
        }
    }

    fn unchanged(&self, before: i64, target: i64) -> Result<GrownRootfs, String> {
        let source = self.source.header();
        let header = DiffMetadata {
            block_size: source.metadata.block_size,
            ..DiffMetadata::default()
        }
        .to_diff_header(source, self.layer_id)
        .map_err(|e| format!("build unchanged rootfs header: {e}"))?;
        validate(&header.mappings, header.metadata.size, PAGE_SIZE)
            .map_err(|e| format!("validate unchanged rootfs mapping: {e}"))?;
        Ok(GrownRootfs {
            diff: RootfsDiff::Unchanged,
            header,
            free: FreeSpace {
                before,
                after: before,
                target,
            },
        })
    }

    async fn resize_and_export(
        &self,
        source_size: i64,
        new_size: i64,
        before: i64,
        target: i64,
    ) -> Result<GrownRootfs, String> {
        let overlay = Overlay::with_size(self.source.clone(), &self.cache_path("grow"), new_size as u64)
            .map(Arc::new)
            .map_err(|e| format!("create grow cache: {e}"))?;
        overlay
            .write_zeroes_at(source_size as u64, (new_size - source_size) as u64)
            .map_err(|e| format!("zero-fill grown tail: {e}"))?;
        let block_size = self.source.header().metadata.block_size as i64;
        let after = resize_offline(overlay.clone(), new_size, block_size, self.deadline).await?;
        let (diff, header) = self.export(&overlay, new_size as u64).await?;
        Ok(GrownRootfs {
            diff,
            header,
            free: FreeSpace { before, after, target },
        })
    }

    async fn export(&self, overlay: &Arc<Overlay>, new_size: u64) -> Result<(RootfsDiff, Header), String> {
        let path = self.dir.join(ROOTFS_DIFF);
        self.export_to(overlay, &path, new_size)
            .await
            .inspect_err(|_| drop(std::fs::remove_file(&path)))
    }

    async fn export_to(
        &self,
        overlay: &Arc<Overlay>,
        path: &Path,
        new_size: u64,
    ) -> Result<(RootfsDiff, Header), String> {
        let diff = self
            .deadline
            .bound(export_diff(overlay.clone(), path.to_path_buf()))
            .await?;
        let header = diff
            .to_resized_diff_header(self.source.header(), self.layer_id, new_size)
            .map_err(|e| format!("build grown rootfs header: {e}"))?;
        let bytes = diff.dirty.len() * diff.block_size;
        Ok((
            RootfsDiff::File {
                path: path.to_path_buf(),
                bytes,
            },
            header,
        ))
    }

    fn cache_path(&self, kind: &str) -> PathBuf {
        self.scratch_dir
            .join(format!("{RESIZE_DISK}-{}-{kind}.cache", self.layer_id))
    }
}

fn target_bytes(target_free_mb: i64) -> Result<i64, String> {
    match (0..=i64::MAX >> MB_SHIFT).contains(&target_free_mb) {
        true => Ok(target_free_mb << MB_SHIFT),
        false => Err(format!("disk target overflows: {target_free_mb} MiB")),
    }
}

async fn export_diff(overlay: Arc<Overlay>, path: PathBuf) -> Result<DiffMetadata, String> {
    tokio::task::spawn_blocking(move || {
        let parent = path.parent().map(std::fs::create_dir_all).transpose();
        let mut file = parent
            .and_then(|_| File::create(&path))
            .map_err(|e| format!("create rootfs diff file: {e}"))?;
        overlay
            .export_diff(&mut file)
            .map_err(|e| format!("export grown overlay: {e}"))
    })
    .await
    .map_err(|e| format!("export grown overlay: {e}"))?
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::block::build_file::{Artifact, DiffStore};
    use crate::storage::backend::Storage;
    use crate::storage::header::{BuildMapping, Metadata};

    #[test]
    fn the_free_disk_target_is_whole_mebibytes_within_the_signed_range() {
        assert_eq!(target_bytes(0), Ok(0));
        assert_eq!(target_bytes(512), Ok(512 << 20));
        assert_eq!(target_bytes(-1), Err("disk target overflows: -1 MiB".to_owned()));
        assert_eq!(
            target_bytes((i64::MAX >> 20) + 1),
            Err(format!("disk target overflows: {} MiB", (i64::MAX >> 20) + 1))
        );
    }

    fn grower(dir: &std::path::Path, source: Header) -> RootfsGrower {
        let storage = Storage::local(&dir.join("store")).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.join("cache")));
        RootfsGrower {
            source: Arc::new(BuildFile::new(source, Artifact::Rootfs, store)),
            layer_id: Uuid::from_bytes([2; 16]),
            scratch_dir: dir.join("scratch"),
            dir: dir.join("layer"),
            deadline: Deadline::after(Duration::from_secs(3600)),
        }
    }

    #[test]
    fn an_unchanged_layer_keeps_every_source_mapping_under_a_new_generation() {
        let dir = tempfile::tempdir().unwrap();
        let parent = Uuid::from_bytes([1; 16]);
        let source = Header::new(
            Metadata::template(parent, 4096, 4 * 4096),
            vec![BuildMapping {
                offset: 0,
                length: 4 * 4096,
                build_id: parent,
                storage_offset: 0,
            }],
        )
        .unwrap();
        let grower = grower(dir.path(), source.clone());

        let grown = grower.unchanged(600 << 20, 512 << 20).unwrap();

        assert_eq!(grown.diff, RootfsDiff::Unchanged);
        assert_eq!(grown.header.mappings, source.mappings);
        assert_eq!(grown.header.metadata.build_id, grower.layer_id);
        assert_eq!(grown.header.metadata.generation, 1);
        assert_eq!(grown.header.metadata.size, source.metadata.size);
        assert_eq!(
            grown.free,
            FreeSpace {
                before: 600 << 20,
                after: 600 << 20,
                target: 512 << 20
            }
        );
    }
}
