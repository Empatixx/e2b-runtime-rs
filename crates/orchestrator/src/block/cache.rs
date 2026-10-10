use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::ops::Range;
use std::os::unix::fs::FileExt;
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
        check_block_count(size, block_size)?;
        Ok(Self {
            file: SparseFile::create(path, size)?,
            block_size,
            tracker: RwLock::new(BlockTracker::default()),
        })
    }

    pub fn adopt(path: &Path, block_size: u64) -> io::Result<Self> {
        let file = SparseFile::adopt(path)?;
        check_block_count(file.size(), block_size)?;
        let mut tracker = BlockTracker::default();
        tracker.set(0..file.size().div_ceil(block_size) as u32, BlockState::Data);
        Ok(Self {
            file,
            block_size,
            tracker: RwLock::new(tracker),
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

    pub fn slice(&self, offset: u64, length: usize) -> io::Result<&[u8]> {
        self.file.slice(offset, length)
    }

    pub fn read_if_present(&self, offset: u64, buf: &mut [u8]) -> io::Result<bool> {
        let tracker = self.tracker.read();
        if !tracker.is_present(self.blocks(offset, buf.len() as u64)) {
            return Ok(false);
        }
        self.file.read_at(offset, buf)?;
        Ok(true)
    }

    pub fn fill(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut tracker = self.tracker.write();
        self.file.write_at(offset, data)?;
        tracker.set(self.blocks(offset, data.len() as u64), BlockState::Data);
        Ok(())
    }

    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String> {
        self.check_aligned(offset, data.len() as u64)?;
        data.chunks(self.block_size as usize)
            .zip((offset..).step_by(self.block_size as usize))
            .take_while(|(_, block_offset)| *block_offset < self.size())
            .try_for_each(|(block, block_offset)| self.write_block(block_offset, block))
    }

    pub fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String> {
        self.check_aligned(offset, length)?;
        self.punch(offset, self.clamp(offset, length))
    }

    pub fn export_diff(&self, out: &mut File) -> io::Result<DiffMetadata> {
        let tracker = self.tracker.read();
        let start = out.stream_position()?;
        let written = tracker
            .data_runs()
            .into_iter()
            .try_fold(start, |position, run| self.export_run(out, run, position))?;
        out.seek(SeekFrom::Start(written))?;
        Ok(DiffMetadata {
            dirty: tracker.data().clone(),
            empty: tracker.zero().clone(),
            block_size: self.block_size,
        })
    }

    pub fn diff_metadata(&self) -> DiffMetadata {
        let tracker = self.tracker.read();
        DiffMetadata {
            dirty: tracker.data().clone(),
            empty: tracker.zero().clone(),
            block_size: self.block_size,
        }
    }

    pub fn export_with(&self, out: &mut File, metadata: &DiffMetadata) -> io::Result<()> {
        let start = out.stream_position()?;
        let written = super::tracker::runs(&metadata.dirty)
            .into_iter()
            .try_fold(start, |position, run| self.export_run(out, run, position))?;
        out.seek(SeekFrom::Start(written)).map(drop)
    }

    fn export_run(&self, out: &File, run: Range<u32>, position: u64) -> io::Result<u64> {
        let from = u64::from(run.start) * self.block_size;
        let length = u64::from(run.end - run.start) * self.block_size;
        let available = self.clamp(from, length);
        self.file.copy_to(out, from, position, available)?;
        out.write_all_at(&vec![0; (length - available) as usize], position + available)?;
        Ok(position + length)
    }

    fn write_block(&self, offset: u64, block: &[u8]) -> Result<(), String> {
        let length = self.clamp(offset, block.len() as u64);
        if block.iter().all(|byte| *byte == 0) {
            return self.punch(offset, length);
        }
        let mut tracker = self.tracker.write();
        self.file
            .write_at(offset, &block[..length as usize])
            .map_err(|e| format!("write at {offset}: {e}"))?;
        tracker.set(self.blocks(offset, length), BlockState::Data);
        Ok(())
    }

    fn punch(&self, offset: u64, length: u64) -> Result<(), String> {
        if length == 0 {
            return Ok(());
        }
        let mut tracker = self.tracker.write();
        self.file
            .punch_hole(offset, length)
            .map_err(|e| format!("punch hole at {offset}: {e}"))?;
        tracker.set(self.blocks(offset, length), BlockState::Zero);
        Ok(())
    }

    fn clamp(&self, offset: u64, length: u64) -> u64 {
        self.size().saturating_sub(offset).min(length)
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
        saturating_index(first)..saturating_index(end)
    }
}

fn saturating_index(block: u64) -> u32 {
    u32::try_from(block).unwrap_or(u32::MAX)
}

fn check_block_count(size: u64, block_size: u64) -> io::Result<()> {
    match size.div_ceil(block_size) > u64::from(u32::MAX) {
        true => Err(io::Error::other(format!(
            "{size} bytes in blocks of {block_size} need more than {} block indexes",
            u32::MAX
        ))),
        false => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

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
    fn a_deferred_export_writes_the_blocks_the_snapshot_saw() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 4);
        cache.write_at(3 * 4096, &[3; 4096]).unwrap();
        cache.write_at(0, &[9; 4096]).unwrap();
        let metadata = cache.diff_metadata();
        cache.write_at(4096, &[7; 4096]).unwrap();

        let mut out = tempfile::tempfile().unwrap();
        cache.export_with(&mut out, &metadata).unwrap();

        assert_eq!(metadata.dirty.iter().collect::<Vec<_>>(), vec![0, 3]);
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

    #[test]
    fn contiguous_dirty_blocks_export_as_one_run_after_the_stream_position() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path(), 6);
        cache
            .write_at(4096, &[[1; 4096], [2; 4096], [3; 4096]].concat())
            .unwrap();
        cache.write_at(5 * 4096, &[4; 4096]).unwrap();

        let mut out = tempfile::tempfile().unwrap();
        out.write_all(&[9; 10]).unwrap();
        cache.export_diff(&mut out).unwrap();

        let expected = [vec![9; 10], vec![1; 4096], vec![2; 4096], vec![3; 4096], vec![4; 4096]].concat();
        assert_eq!(contents(&mut out), expected);
        assert_eq!(out.stream_position().unwrap(), expected.len() as u64);
    }

    #[test]
    fn a_partial_last_block_exports_zero_padded_to_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::create(&dir.path().join("cache"), 5000, 4096).unwrap();
        cache.fill(0, &[4; 5000]).unwrap();

        let mut out = tempfile::tempfile().unwrap();
        cache.export_diff(&mut out).unwrap();

        assert_eq!(contents(&mut out), [vec![4; 5000], vec![0; 3192]].concat());
    }

    #[test]
    fn writes_past_the_size_are_dropped_and_the_cache_file_keeps_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        let cache = BlockCache::create(&path, 2 * 4096, 4096).unwrap();

        cache.write_at(4096, &[[5; 4096], [6; 4096]].concat()).unwrap();
        cache.write_at(4 * 4096, &[7; 4096]).unwrap();
        cache.write_zeroes_at(8 * 4096, 4096).unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * 4096);
        assert!(cache.is_present(4096, 4096));
        assert!(!cache.is_present(2 * 4096, 4096));
        let mut out = tempfile::tempfile().unwrap();
        let diff = cache.export_diff(&mut out).unwrap();
        assert_eq!(diff.dirty.iter().collect::<Vec<_>>(), vec![1]);
        assert!(diff.empty.is_empty());
    }

    #[test]
    fn a_cache_with_more_blocks_than_the_index_holds_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = BlockCache::create(&dir.path().join("cache"), (1 << 32) * 4096, 4096)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "17592186044416 bytes in blocks of 4096 need more than 4294967295 block indexes"
        );
    }

    #[test]
    fn concurrent_data_and_zero_writes_keep_tracker_and_file_in_step() {
        let dir = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(cache(dir.path(), 1));
        for _ in 0..200 {
            let writers: Vec<_> = [vec![6; 4096], vec![0; 4096]]
                .into_iter()
                .map(|block| {
                    let cache = cache.clone();
                    std::thread::spawn(move || cache.write_at(0, &block).unwrap())
                })
                .collect();
            writers.into_iter().for_each(|writer| writer.join().unwrap());

            let mut block = [9; 4096];
            cache.read_at(0, &mut block).unwrap();
            let zero_in_file = block.iter().all(|byte| *byte == 0);
            let zero_in_tracker = cache.tracker.read().state(0) == Some(BlockState::Zero);
            assert_eq!(zero_in_file, zero_in_tracker);
        }
    }
}
