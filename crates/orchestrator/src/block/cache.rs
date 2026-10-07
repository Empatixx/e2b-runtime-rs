use std::fs::File;
use std::io::{self, Write};
use std::ops::Range;
use std::path::Path;

use parking_lot::RwLock;

use super::sparse_file::SparseFile;
use super::tracker::{BlockState, BlockTracker};
use crate::storage::header::DiffMetadata;

pub struct BlockCache {
    file: SparseFile,
    block_size: u64,
    tracker: RwLock<BlockTracker>,
}

impl BlockCache {
    pub fn create(path: &Path, size: u64, block_size: u64) -> io::Result<Self> {
        Ok(Self {
            file: SparseFile::create(path, size)?,
            block_size,
            tracker: RwLock::new(BlockTracker::default()),
        })
    }

    pub fn size(&self) -> u64 {
        self.file.size()
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    pub fn is_present(&self, offset: u64, length: u64) -> bool {
        self.tracker.read().is_present(self.blocks(offset, length))
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_at(offset, buf)
    }

    pub fn fill(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write_at(offset, data)?;
        let blocks = self.blocks(offset, data.len() as u64);
        self.tracker.write().set(blocks, BlockState::Data);
        Ok(())
    }

    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String> {
        self.check_aligned(offset, data.len() as u64)?;
        data.chunks(self.block_size as usize)
            .zip((offset..).step_by(self.block_size as usize))
            .try_for_each(|(block, block_offset)| self.write_block(block_offset, block))
    }

    pub fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String> {
        self.check_aligned(offset, length)?;
        self.file
            .punch_hole(offset, length)
            .map_err(|e| format!("punch hole at {offset}: {e}"))?;
        self.tracker.write().set(self.blocks(offset, length), BlockState::Zero);
        Ok(())
    }

    pub fn export_diff(&self, out: &mut File) -> io::Result<DiffMetadata> {
        let tracker = self.tracker.read();
        let mut block = vec![0; self.block_size as usize];
        for index in tracker.data() {
            self.file.read_at(u64::from(index) * self.block_size, &mut block)?;
            out.write_all(&block)?;
        }
        Ok(DiffMetadata {
            dirty: tracker.data().clone(),
            empty: tracker.zero().clone(),
            block_size: self.block_size,
        })
    }

    fn write_block(&self, offset: u64, block: &[u8]) -> Result<(), String> {
        if block.iter().all(|byte| *byte == 0) {
            return self.write_zeroes_at(offset, block.len() as u64);
        }
        self.file
            .write_at(offset, block)
            .map_err(|e| format!("write at {offset}: {e}"))?;
        self.tracker
            .write()
            .set(self.blocks(offset, block.len() as u64), BlockState::Data);
        Ok(())
    }

    fn check_aligned(&self, offset: u64, length: u64) -> Result<(), String> {
        let aligned = offset.is_multiple_of(self.block_size) && length.is_multiple_of(self.block_size);
        if aligned {
            return Ok(());
        }
        Err(format!(
            "misaligned write: offset {offset} length {length} block size {}",
            self.block_size
        ))
    }

    fn blocks(&self, offset: u64, length: u64) -> Range<u32> {
        let first = offset / self.block_size;
        let end = (offset + length).div_ceil(self.block_size);
        first as u32..end as u32
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use super::*;

    fn cache(dir: &Path, blocks: u64) -> BlockCache {
        BlockCache::create(&dir.join("cache"), blocks * 4096, 4096).unwrap()
    }

    fn contents(file: &mut File) -> Vec<u8> {
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn rejects_misaligned_writes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 4);
        assert_eq!(
            cache.write_at(100, &[1; 4096]).unwrap_err(),
            "misaligned write: offset 100 length 4096 block size 4096"
        );
        assert_eq!(
            cache.write_zeroes_at(0, 10).unwrap_err(),
            "misaligned write: offset 0 length 10 block size 4096"
        );
    }

    #[test]
    fn zero_blocks_become_empty_and_data_blocks_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 4);
        cache.write_at(0, &[vec![1; 4096], vec![0; 4096]].concat()).unwrap();

        assert!(cache.is_present(0, 8192));
        assert!(!cache.is_present(8192, 4096));

        let mut out = tempfile::tempfile().unwrap();
        let diff = cache.export_diff(&mut out).unwrap();
        assert_eq!(diff.dirty.iter().collect::<Vec<_>>(), vec![0]);
        assert_eq!(diff.empty.iter().collect::<Vec<_>>(), vec![1]);
        assert_eq!(diff.block_size, 4096);
        assert_eq!(contents(&mut out), vec![1; 4096]);
    }

    #[test]
    fn export_packs_non_contiguous_dirty_blocks_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 4);
        cache.write_at(3 * 4096, &[3; 4096]).unwrap();
        cache.write_at(0, &[9; 4096]).unwrap();

        let mut out = tempfile::tempfile().unwrap();
        let diff = cache.export_diff(&mut out).unwrap();
        assert_eq!(diff.dirty.iter().collect::<Vec<_>>(), vec![0, 3]);
        assert_eq!(contents(&mut out), [vec![9; 4096], vec![3; 4096]].concat());
    }

    #[test]
    fn write_zeroes_overrides_earlier_data() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 4);
        cache.write_at(4096, &[5; 4096]).unwrap();
        cache.write_zeroes_at(4096, 4096).unwrap();

        let mut block = [9; 4096];
        cache.read_at(4096, &mut block).unwrap();
        assert_eq!(block, [0; 4096]);

        let mut out = tempfile::tempfile().unwrap();
        let diff = cache.export_diff(&mut out).unwrap();
        assert!(diff.dirty.is_empty());
        assert_eq!(diff.empty.iter().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn fill_marks_a_partial_last_block_present() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::create(&dir.path().join("cache"), 5000, 4096).unwrap();
        cache.fill(0, &[4; 5000]).unwrap();
        assert!(cache.is_present(0, 5000));

        let mut tail = [0; 904];
        cache.read_at(4096, &mut tail).unwrap();
        assert_eq!(tail, [4; 904]);
    }
}
