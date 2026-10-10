use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::build_file::BuildFile;
use super::cache::BlockCache;
use crate::storage::header::DiffMetadata;

pub struct Overlay {
    base: Arc<BuildFile>,
    cache: BlockCache,
}

impl Overlay {
    pub fn new(base: Arc<BuildFile>, cache_path: &Path) -> io::Result<Self> {
        let size = base.size();
        Self::with_size(base, cache_path, size)
    }

    pub fn with_size(base: Arc<BuildFile>, cache_path: &Path, size: u64) -> io::Result<Self> {
        let cache = BlockCache::create(cache_path, size, base.block_size())?;
        Ok(Self { base, cache })
    }

    pub fn size(&self) -> u64 {
        self.cache.size()
    }

    pub fn block_size(&self) -> u64 {
        self.cache.block_size()
    }

    pub fn base(&self) -> &Arc<BuildFile> {
        &self.base
    }

    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        self.check_aligned(offset, buf.len() as u64)?;
        let block_size = self.cache.block_size();
        for (block, block_offset) in buf
            .chunks_mut(block_size as usize)
            .zip((offset..).step_by(block_size as usize))
        {
            self.read_block(block_offset, block).await?;
        }
        Ok(())
    }

    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String> {
        self.cache.write_at(offset, data)
    }

    pub fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String> {
        self.cache.write_zeroes_at(offset, length)
    }

    pub fn export_diff(&self, out: &mut File) -> io::Result<DiffMetadata> {
        self.cache.export_diff(out)
    }

    pub fn diff_metadata(&self) -> DiffMetadata {
        self.cache.diff_metadata()
    }

    pub fn export_with(&self, out: &mut File, metadata: &DiffMetadata) -> io::Result<()> {
        self.cache.export_with(out, metadata)
    }

    async fn read_block(&self, offset: u64, block: &mut [u8]) -> Result<(), String> {
        let cached = self
            .cache
            .read_if_present(offset, block)
            .map_err(|e| format!("read overlay cache at {offset}: {e}"))?;
        if cached {
            return Ok(());
        }
        self.base.read_at(offset, block).await
    }

    fn check_aligned(&self, offset: u64, length: u64) -> Result<(), String> {
        let block_size = self.cache.block_size();
        if offset.is_multiple_of(block_size) && length.is_multiple_of(block_size) {
            return Ok(());
        }
        Err(format!(
            "misaligned read: offset {offset} length {length} block size {block_size}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use bytes::Bytes;
    use uuid::Uuid;

    use super::*;
    use crate::block::build_file::{Artifact, DiffStore};
    use crate::storage::backend::Storage;
    use crate::storage::header::{Header, Metadata};

    const SIZE: u64 = 4 * 4096;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn template_bytes() -> Vec<u8> {
        (1..=4u8).flat_map(|value| vec![value; 4096]).collect()
    }

    async fn template(dir: &Path) -> (Storage, Header) {
        let storage = Storage::local(&dir.join("store")).unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(1)), Bytes::from(template_bytes()))
            .await
            .unwrap();
        let header = Header::new(Metadata::template(id(1), 4096, SIZE), Vec::new()).unwrap();
        (storage, header)
    }

    async fn read_all(read: impl AsyncFnOnce(&mut [u8]) -> Result<(), String>) -> Vec<u8> {
        let mut bytes = vec![0; SIZE as usize];
        read(&mut bytes).await.unwrap();
        bytes
    }

    #[tokio::test]
    async fn pause_and_resume_round_trip_through_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let (storage, template) = template(dir.path()).await;
        let store = Arc::new(DiffStore::new(storage.clone(), dir.path().join("cache")));
        let base = BuildFile::new(template.clone(), Artifact::Rootfs, store.clone());
        let overlay = Overlay::new(Arc::new(base), &dir.path().join("overlay")).unwrap();

        overlay.write_at(4096, &[7; 4096]).unwrap();
        overlay.write_zeroes_at(8192, 4096).unwrap();
        let before_pause = read_all(async |bytes| overlay.read_at(0, bytes).await).await;
        let expected = [vec![1; 4096], vec![7; 4096], vec![0; 4096], vec![4; 4096]].concat();
        assert_eq!(before_pause, expected);

        let mut diff_file = tempfile::tempfile().unwrap();
        let diff = overlay.export_diff(&mut diff_file).unwrap();
        let mut diff_bytes = Vec::new();
        diff_file.seek(SeekFrom::Start(0)).unwrap();
        diff_file.read_to_end(&mut diff_bytes).unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(2)), Bytes::from(diff_bytes))
            .await
            .unwrap();

        let header = diff.to_diff_header(&template, id(2)).unwrap();
        let resumed = BuildFile::new(header, Artifact::Rootfs, store);
        let after_resume = read_all(async |bytes| resumed.read_at(0, bytes).await).await;
        assert_eq!(after_resume, before_pause);
    }

    #[tokio::test]
    async fn a_grown_overlay_reads_its_zeroed_tail_and_exports_it_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (storage, template) = template(dir.path()).await;
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let base = BuildFile::new(template, Artifact::Rootfs, store);
        let overlay = Overlay::with_size(Arc::new(base), &dir.path().join("overlay"), SIZE + 8192).unwrap();

        overlay.write_zeroes_at(SIZE, 8192).unwrap();
        overlay.write_at(SIZE + 4096, &[9; 4096]).unwrap();

        let mut tail = vec![5; 8192];
        overlay.read_at(SIZE, &mut tail).await.unwrap();
        assert_eq!(overlay.size(), SIZE + 8192);
        assert_eq!(tail, [vec![0; 4096], vec![9; 4096]].concat());
        let mut diff_file = tempfile::tempfile().unwrap();
        let diff = overlay.export_diff(&mut diff_file).unwrap();
        assert_eq!(diff.dirty.iter().collect::<Vec<_>>(), vec![5]);
        assert_eq!(diff.empty.iter().collect::<Vec<_>>(), vec![4]);
    }

    #[tokio::test]
    async fn rejects_misaligned_reads() {
        let dir = tempfile::tempdir().unwrap();
        let (storage, template) = template(dir.path()).await;
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let base = BuildFile::new(template, Artifact::Rootfs, store);
        let overlay = Overlay::new(Arc::new(base), &dir.path().join("overlay")).unwrap();

        let mut buf = vec![0; 4096];
        assert_eq!(
            overlay.read_at(100, &mut buf).await.unwrap_err(),
            "misaligned read: offset 100 length 4096 block size 4096"
        );
    }

    #[tokio::test]
    async fn the_block_size_comes_from_the_base_header() {
        let dir = tempfile::tempdir().unwrap();
        let (storage, _) = template(dir.path()).await;
        let header = Header::new(Metadata::template(id(1), 8192, SIZE), Vec::new()).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let base = BuildFile::new(header, Artifact::Rootfs, store);
        let overlay = Overlay::new(Arc::new(base), &dir.path().join("overlay")).unwrap();

        let mut buf = vec![0; 4096];
        assert_eq!(overlay.block_size(), 8192);
        assert_eq!(
            overlay.read_at(4096, &mut buf).await.unwrap_err(),
            "misaligned read: offset 4096 length 4096 block size 8192"
        );
    }
}
