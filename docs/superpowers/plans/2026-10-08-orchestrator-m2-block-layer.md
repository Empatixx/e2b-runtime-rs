# Orchestrator M2: Block Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the block layer. It reads the bytes of any build artifact (memfile or rootfs) lazily from object storage, in 4 MiB chunks cached on local disk, and resolves them through the header mapping. It also provides a copy-on-write overlay whose writes can be exported as a diff and resumed as a new build.

**Architecture:** Five small types in `crates/orchestrator/src/block/`, each with one job:

| Type | Job |
|---|---|
| `SparseFile` | local sparse file with positional reads, writes and hole punching; deleted on drop |
| `BlockTracker` | which blocks hold data and which are zero (two roaring bitmaps) |
| `BlockCache` | `SparseFile` + `BlockTracker`; aligned writes, presence checks, diff export |
| `Chunker` | a `BlockCache` over one storage object; fetches whole 4 MiB chunks once, shared by concurrent readers |
| `BuildFile` | a device view of a build; resolves offsets through `Header::locate` into per-build `Chunker`s and zero-fills nil mappings |
| `Overlay` | copy-on-write device: reads come from the cache if the block was written, otherwise from the base `BuildFile`; writes go to the cache |

Everything uses positional file I/O (`pread` / `pwrite` via `FileExt`) instead of `mmap`, so M2 has no `unsafe`. Zero-copy mapping comes with uffd in M6.

**Tech Stack:** Rust 1.98, tokio, roaring, parking_lot, nix (`fallocate`), object_store via `storage::backend::Storage`, tempfile for tests.

**Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`, milestone 2.

## Global Constraints

- Code style follows `CLAUDE.md`:
  - readability first: small well-named methods with low cognitive complexity;
  - method chains, builders where a value is created in several places, named fields;
  - no comments, no `.0` access, crates.io module layout;
  - low-level details hidden behind small safe types.
- Block size for caches is `PAGE_SIZE` (4096). The chunk size is `4 << 20` (4 MiB), and the last chunk of an object may be shorter.
- A chunk is marked present only after the whole chunk has been written to the cache. A failed fetch leaves nothing marked, so the next read retries.
- A diff export writes only data blocks, packed in ascending block order. Zero blocks go to `DiffMetadata.empty`. This matches upstream `ExportToDiff` and `header::mapping::from_blocks`.
- Errors are `String`. Cache files are deleted when their owner is dropped.
- Tests must run on macOS and Linux without KVM or root.
- Before every commit run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test -p orchestrator`.

## Review Focus

1. **A read that spans two chunks, or a chunk and the shorter last chunk,** must return the right bytes and fetch each chunk exactly once. Covered in Task 3.
2. **Many concurrent readers of one missing chunk** must trigger one storage fetch, not one per reader. Covered in Task 3.
3. **A failed or timed-out fetch** must surface an error and must not leave the chunk marked present, so a later read succeeds once storage is back. Covered in Task 3.
4. **An unaligned read that crosses a mapping boundary** (end of a base mapping into a diff mapping, or into a nil mapping) must stitch the bytes correctly. Covered in Task 4.
5. **Writes, zero writes, export and resume** must give back exactly the bytes the overlay showed before the pause. Covered in Task 5.

---

### Task 1: SparseFile and BlockTracker

**Files:**
- Create: `crates/orchestrator/src/block.rs`, `crates/orchestrator/src/block/sparse_file.rs`, `crates/orchestrator/src/block/tracker.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`pub mod block;`), `crates/orchestrator/Cargo.toml` (`nix = { version = "0.29", features = ["fs"] }`, `parking_lot = "0.12"`)

**Interfaces:**
- Produces:
  - `block::sparse_file::SparseFile` with:
    - `create(path: &Path, size: u64) -> io::Result<SparseFile>` (creates parent directories)
    - `size(&self) -> u64`
    - `read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>`
    - `write_at(&self, offset: u64, data: &[u8]) -> io::Result<()>`
    - `punch_hole(&self, offset: u64, length: u64) -> io::Result<()>`
    - Drop removes the file.
  - `block::tracker::BlockState { Data, Zero }`
  - `block::tracker::BlockTracker` (Default) with:
    - `set(&mut self, blocks: Range<u32>, state: BlockState)`
    - `state(&self, block: u32) -> Option<BlockState>`
    - `is_present(&self, blocks: Range<u32>) -> bool`
    - `data(&self) -> &RoaringBitmap`
    - `zero(&self) -> &RoaringBitmap`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/block.rs`:
```rust
pub mod sparse_file;
pub mod tracker;
```
Add `pub mod block;` to `crates/orchestrator/src/lib.rs` (above `pub mod storage;`). Run `cargo add -p orchestrator nix@0.29 --features fs` and `cargo add -p orchestrator parking_lot@0.12`.

`crates/orchestrator/src/block/sparse_file.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_writes_and_punched_holes_read_as_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/cache");
        let file = SparseFile::create(&path, 16384).unwrap();

        file.write_at(4096, &[7; 4096]).unwrap();
        let mut block = [0; 4096];
        file.read_at(4096, &mut block).unwrap();
        assert_eq!(block, [7; 4096]);

        file.punch_hole(4096, 4096).unwrap();
        file.read_at(4096, &mut block).unwrap();
        assert_eq!(block, [0; 4096]);
        assert_eq!(file.size(), 16384);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16384);
    }

    #[test]
    fn file_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        drop(SparseFile::create(&path, 4096).unwrap());
        assert!(!path.exists());
    }
}
```

`crates/orchestrator/src/block/tracker.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_replace_each_other_and_presence_needs_every_block() {
        let mut tracker = BlockTracker::default();
        tracker.set(0..2, BlockState::Data);
        tracker.set(1..3, BlockState::Zero);

        assert_eq!(tracker.state(0), Some(BlockState::Data));
        assert_eq!(tracker.state(1), Some(BlockState::Zero));
        assert_eq!(tracker.state(3), None);
        assert!(tracker.is_present(0..3));
        assert!(!tracker.is_present(2..4));
        assert!(tracker.is_present(5..5));
        assert_eq!(tracker.data().iter().collect::<Vec<_>>(), vec![0]);
        assert_eq!(tracker.zero().iter().collect::<Vec<_>>(), vec![1, 2]);
    }
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator block::`
Expected: FAIL to compile, `cannot find type SparseFile` and `cannot find type BlockTracker`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/block/sparse_file.rs`:
```rust
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub struct SparseFile {
    file: File,
    path: PathBuf,
    size: u64,
}

impl SparseFile {
    pub fn create(path: &Path, size: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(size)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            size,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    pub fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write_all_at(data, offset)
    }

    pub fn punch_hole(&self, offset: u64, length: u64) -> io::Result<()> {
        punch_hole(&self.file, offset, length)
    }
}

impl Drop for SparseFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(target_os = "linux")]
fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    use nix::fcntl::{FallocateFlags, fallocate};

    let mode = FallocateFlags::FALLOC_FL_PUNCH_HOLE | FallocateFlags::FALLOC_FL_KEEP_SIZE;
    fallocate(file.as_raw_fd(), mode, offset as i64, length as i64).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    file.write_all_at(&vec![0; length as usize], offset)
}
```

Put at the top of `crates/orchestrator/src/block/tracker.rs`:
```rust
use std::ops::Range;

use roaring::RoaringBitmap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Data,
    Zero,
}

#[derive(Debug, Clone, Default)]
pub struct BlockTracker {
    data: RoaringBitmap,
    zero: RoaringBitmap,
}

impl BlockTracker {
    pub fn set(&mut self, blocks: Range<u32>, state: BlockState) {
        let (target, other) = match state {
            BlockState::Data => (&mut self.data, &mut self.zero),
            BlockState::Zero => (&mut self.zero, &mut self.data),
        };
        other.remove_range(blocks.clone());
        target.insert_range(blocks);
    }

    pub fn state(&self, block: u32) -> Option<BlockState> {
        if self.data.contains(block) {
            Some(BlockState::Data)
        } else if self.zero.contains(block) {
            Some(BlockState::Zero)
        } else {
            None
        }
    }

    pub fn is_present(&self, blocks: Range<u32>) -> bool {
        blocks.into_iter().all(|block| self.state(block).is_some())
    }

    pub fn data(&self) -> &RoaringBitmap {
        &self.data
    }

    pub fn zero(&self) -> &RoaringBitmap {
        &self.zero
    }
}
```

If nix 0.29's `fallocate` takes `impl AsFd` instead of a raw fd, pass `file` directly and record it in the ledger.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator block::`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator Cargo.lock
git commit -m "feat(orchestrator): sparse cache file and block state tracker"
```

---

### Task 2: BlockCache

**Files:**
- Create: `crates/orchestrator/src/block/cache.rs`
- Modify: `crates/orchestrator/src/block.rs` (`pub mod cache;`)

**Interfaces:**
- Consumes: `SparseFile`, `BlockTracker`, `BlockState` (Task 1); `storage::header::DiffMetadata`.
- Produces: `block::cache::BlockCache` with:
  - `create(path: &Path, size: u64, block_size: u64) -> io::Result<BlockCache>`
  - `size(&self) -> u64`
  - `block_size(&self) -> u64`
  - `is_present(&self, offset: u64, length: u64) -> bool`
  - `read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>` (no presence check)
  - `fill(&self, offset: u64, data: &[u8]) -> io::Result<()>` (writes fetched bytes and marks the covered blocks `Data`; may end on a partial last block)
  - `write_at(&self, offset: u64, data: &[u8]) -> Result<(), String>` (block-aligned; all-zero blocks are punched and marked `Zero`)
  - `write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String>`
  - `export_diff(&self, out: &mut File) -> io::Result<DiffMetadata>`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/block/cache.rs`:
```rust
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
```
Add `pub mod cache;` to `crates/orchestrator/src/block.rs`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator block::cache`
Expected: FAIL to compile, `cannot find type BlockCache`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/block/cache.rs`:
```rust
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
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator block::cache`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator
git commit -m "feat(orchestrator): block cache with aligned writes and diff export"
```

---

### Task 3: Chunker

**Files:**
- Create: `crates/orchestrator/src/block/chunker.rs`
- Modify: `crates/orchestrator/src/block.rs` (`pub mod chunker;`)

**Interfaces:**
- Consumes: `BlockCache` (Task 2); `storage::backend::Storage` (`size`, `get_range`, `put`, `delete_prefix`); `storage::header::PAGE_SIZE`.
- Produces:
  - `block::chunker::CHUNK_SIZE: u64 = 4 << 20`
  - `block::chunker::Chunker` with:
    - `async open(storage: Storage, object: &str, cache_path: &Path) -> Result<Chunker, String>`
    - `size(&self) -> u64`
    - `fetch_count(&self) -> u64`
    - `async read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String>`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/block/chunker.rs`:
```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use tempfile::TempDir;

    use super::*;

    const MIB: u64 = 1 << 20;
    const OBJECT: &str = "build/memfile";

    fn pattern(length: u64) -> Vec<u8> {
        (0..length).map(|index| (index % 251) as u8).collect()
    }

    async fn setup(length: u64) -> (TempDir, Storage, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let data = pattern(length);
        storage.put(OBJECT, Bytes::from(data.clone())).await.unwrap();
        (dir, storage, data)
    }

    async fn chunker(dir: &TempDir, storage: Storage) -> Chunker {
        Chunker::open(storage, OBJECT, &dir.path().join("cache")).await.unwrap()
    }

    #[tokio::test]
    async fn reads_any_range_and_fetches_each_chunk_once() {
        let (dir, storage, data) = setup(9 * MIB).await;
        let chunker = chunker(&dir, storage).await;

        let mut across = vec![0; 8192];
        chunker.read_at(4 * MIB - 4096, &mut across).await.unwrap();
        assert_eq!(across, data[(4 * MIB - 4096) as usize..][..8192]);
        assert_eq!(chunker.fetch_count(), 2);

        let mut cached = vec![0; 100];
        chunker.read_at(100, &mut cached).await.unwrap();
        assert_eq!(cached, data[100..200]);
        assert_eq!(chunker.fetch_count(), 2);

        let mut tail = vec![0; 1000];
        chunker.read_at(9 * MIB - 1000, &mut tail).await.unwrap();
        assert_eq!(tail, data[(9 * MIB - 1000) as usize..]);
        assert_eq!(chunker.fetch_count(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_readers_share_one_fetch() {
        let (dir, storage, data) = setup(4 * MIB).await;
        let chunker = Arc::new(chunker(&dir, storage).await);

        let reads = (0..8u64).map(|index| {
            let chunker = chunker.clone();
            tokio::spawn(async move {
                let mut block = vec![0; 4096];
                chunker.read_at(index * 4096, &mut block).await.unwrap();
                block
            })
        });
        let blocks = futures::future::join_all(reads).await;

        for (index, block) in blocks.into_iter().enumerate() {
            assert_eq!(block.unwrap(), data[index * 4096..][..4096]);
        }
        assert_eq!(chunker.fetch_count(), 1);
    }

    #[tokio::test]
    async fn failed_fetch_is_retried_on_the_next_read() {
        let (dir, storage, data) = setup(4 * MIB).await;
        let chunker = chunker(&dir, storage.clone()).await;
        storage.delete_prefix("build").await.unwrap();

        let mut block = vec![0; 4096];
        let error = chunker.read_at(0, &mut block).await.unwrap_err();
        assert!(error.starts_with("get build/memfile"), "{error}");
        assert_eq!(chunker.fetch_count(), 0);

        storage.put(OBJECT, Bytes::from(data.clone())).await.unwrap();
        chunker.read_at(0, &mut block).await.unwrap();
        assert_eq!(block, data[..4096]);
        assert_eq!(chunker.fetch_count(), 1);
    }

    #[tokio::test]
    async fn read_past_the_end_is_an_error() {
        let (dir, storage, _) = setup(4 * MIB).await;
        let chunker = chunker(&dir, storage).await;
        let mut buf = vec![0; 8192];
        assert_eq!(
            chunker.read_at(4 * MIB - 4096, &mut buf).await.unwrap_err(),
            "read 4190208..4198400 is past the end of build/memfile (4194304 bytes)"
        );
    }
}
```
Add `pub mod chunker;` to `crates/orchestrator/src/block.rs`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator block::chunker`
Expected: FAIL to compile, `cannot find type Chunker`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/block/chunker.rs`:
```rust
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use super::cache::BlockCache;
use crate::storage::backend::Storage;
use crate::storage::header::PAGE_SIZE;

pub const CHUNK_SIZE: u64 = 4 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Chunker {
    cache: BlockCache,
    storage: Storage,
    object: String,
    chunk_locks: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
    fetches: AtomicU64,
}

impl Chunker {
    pub async fn open(storage: Storage, object: &str, cache_path: &Path) -> Result<Self, String> {
        let size = storage.size(object).await?;
        let cache = BlockCache::create(cache_path, size, PAGE_SIZE)
            .map_err(|e| format!("create cache {}: {e}", cache_path.display()))?;
        Ok(Self {
            cache,
            storage,
            object: object.to_owned(),
            chunk_locks: Mutex::default(),
            fetches: AtomicU64::new(0),
        })
    }

    pub fn size(&self) -> u64 {
        self.cache.size()
    }

    pub fn fetch_count(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }

    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let end = offset + buf.len() as u64;
        if end > self.size() {
            return Err(format!(
                "read {offset}..{end} is past the end of {} ({} bytes)",
                self.object,
                self.size()
            ));
        }
        for chunk in chunks_covering(offset..end) {
            self.ensure_chunk(chunk).await?;
        }
        self.cache
            .read_at(offset, buf)
            .map_err(|e| format!("read cache of {} at {offset}: {e}", self.object))
    }

    async fn ensure_chunk(&self, chunk: u64) -> Result<(), String> {
        let range = self.chunk_range(chunk);
        if self.is_cached(&range) {
            return Ok(());
        }
        let lock = self.chunk_lock(chunk);
        let _guard = lock.lock().await;
        if self.is_cached(&range) {
            return Ok(());
        }
        self.fetch(range).await
    }

    async fn fetch(&self, range: Range<u64>) -> Result<(), String> {
        let bytes = tokio::time::timeout(FETCH_TIMEOUT, self.storage.get_range(&self.object, range.clone()))
            .await
            .map_err(|_| format!("fetch {} {range:?}: timed out after 60s", self.object))??;
        self.cache
            .fill(range.start, &bytes)
            .map_err(|e| format!("fill cache of {} {range:?}: {e}", self.object))?;
        self.fetches.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn is_cached(&self, range: &Range<u64>) -> bool {
        self.cache.is_present(range.start, range.end - range.start)
    }

    fn chunk_range(&self, chunk: u64) -> Range<u64> {
        let start = chunk * CHUNK_SIZE;
        start..(start + CHUNK_SIZE).min(self.size())
    }

    fn chunk_lock(&self, chunk: u64) -> Arc<tokio::sync::Mutex<()>> {
        self.chunk_locks.lock().entry(chunk).or_default().clone()
    }
}

fn chunks_covering(range: Range<u64>) -> Range<u64> {
    range.start / CHUNK_SIZE..range.end.div_ceil(CHUNK_SIZE)
}
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator block::chunker`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator
git commit -m "feat(orchestrator): chunker fetches 4 MiB chunks once into a local cache"
```

---

### Task 4: BuildFile

**Files:**
- Create: `crates/orchestrator/src/block/build_file.rs`
- Modify: `crates/orchestrator/src/block.rs` (`pub mod build_file;`)

**Interfaces:**
- Consumes: `Chunker` (Task 3); `Header`, `Location`, `BuildMapping`, `Metadata` (M1); `BuildPaths` (M1); `Storage`.
- Produces:
  - `block::build_file::Artifact { Memfile, Rootfs }` with `object(&self, build_id: Uuid) -> String` and `name(&self) -> &'static str`.
  - `block::build_file::BuildFile` with:
    - `new(header: Header, artifact: Artifact, storage: Storage, cache_dir: PathBuf) -> BuildFile`
    - `header(&self) -> &Header`
    - `size(&self) -> u64`
    - `async read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String>`

- [ ] **Step 1: Write the failing test**

`crates/orchestrator/src/block/build_file.rs`:
```rust
#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::storage::header::{BuildMapping, Metadata};

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn mapping(offset: u64, build_id: Uuid, storage_offset: u64) -> BuildMapping {
        BuildMapping {
            offset,
            length: 4096,
            build_id,
            storage_offset,
        }
    }

    #[tokio::test]
    async fn reads_through_mappings_from_several_builds_and_zero_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(1)), Bytes::from(vec![1; 4 * 4096]))
            .await
            .unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(2)), Bytes::from(vec![2; 4096]))
            .await
            .unwrap();

        let metadata = Metadata::template(id(1), 4096, 4 * 4096).next_generation(id(2));
        let mappings = vec![
            mapping(0, id(1), 0),
            mapping(4096, id(2), 0),
            mapping(8192, Uuid::nil(), 0),
            mapping(12288, id(1), 12288),
        ];
        let header = Header::new(metadata, mappings).unwrap();
        let file = BuildFile::new(header, Artifact::Rootfs, storage, dir.path().join("cache"));

        let mut whole = vec![9; 4 * 4096];
        file.read_at(0, &mut whole).await.unwrap();
        let expected = [vec![1; 4096], vec![2; 4096], vec![0; 4096], vec![1; 4096]].concat();
        assert_eq!(whole, expected);

        let mut across = vec![0; 200];
        file.read_at(4000, &mut across).await.unwrap();
        assert_eq!(across, [vec![1; 96], vec![2; 104]].concat());

        let mut past = vec![0; 10];
        assert_eq!(
            file.read_at(4 * 4096, &mut past).await.unwrap_err(),
            "offset 16384 is out of bounds (size 16384)"
        );
    }
}
```
Add `pub mod build_file;` to `crates/orchestrator/src/block.rs`.

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p orchestrator block::build_file`
Expected: FAIL to compile, `cannot find type BuildFile`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/block/build_file.rs`:
```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use super::chunker::Chunker;
use crate::storage::backend::Storage;
use crate::storage::header::Header;
use crate::storage::paths::BuildPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    Memfile,
    Rootfs,
}

impl Artifact {
    pub fn object(&self, build_id: Uuid) -> String {
        let paths = BuildPaths::new(build_id);
        match self {
            Artifact::Memfile => paths.memfile(),
            Artifact::Rootfs => paths.rootfs(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Artifact::Memfile => "memfile",
            Artifact::Rootfs => "rootfs",
        }
    }
}

pub struct BuildFile {
    header: Header,
    artifact: Artifact,
    storage: Storage,
    cache_dir: PathBuf,
    diffs: tokio::sync::Mutex<HashMap<Uuid, Arc<Chunker>>>,
}

impl BuildFile {
    pub fn new(header: Header, artifact: Artifact, storage: Storage, cache_dir: PathBuf) -> Self {
        Self {
            header,
            artifact,
            storage,
            cache_dir,
            diffs: tokio::sync::Mutex::default(),
        }
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn size(&self) -> u64 {
        self.header.metadata.size
    }

    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let mut done = 0;
        while done < buf.len() {
            let location = self.header.locate(offset + done as u64)?;
            let length = (location.length as usize).min(buf.len() - done);
            let target = &mut buf[done..done + length];
            if location.build_id.is_nil() {
                target.fill(0);
            } else {
                self.diff(location.build_id)
                    .await?
                    .read_at(location.storage_offset, target)
                    .await?;
            }
            done += length;
        }
        Ok(())
    }

    async fn diff(&self, build_id: Uuid) -> Result<Arc<Chunker>, String> {
        let mut diffs = self.diffs.lock().await;
        if let Some(chunker) = diffs.get(&build_id) {
            return Ok(chunker.clone());
        }
        let cache_path = self.cache_dir.join(format!("{build_id}-{}", self.artifact.name()));
        let object = self.artifact.object(build_id);
        let chunker = Arc::new(Chunker::open(self.storage.clone(), &object, &cache_path).await?);
        diffs.insert(build_id, chunker.clone());
        Ok(chunker)
    }
}
```

- [ ] **Step 4: Run the test to see it pass**

Run: `cargo test -p orchestrator block::build_file`
Expected: PASS, 1 test.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator
git commit -m "feat(orchestrator): build file reads through header mappings and per-build chunkers"
```

---

### Task 5: Overlay and the pause/resume round trip

**Files:**
- Create: `crates/orchestrator/src/block/overlay.rs`
- Modify: `crates/orchestrator/src/block.rs` (`pub mod overlay;`)

**Interfaces:**
- Consumes: `BuildFile`, `Artifact` (Task 4); `BlockCache` (Task 2); `DiffMetadata::to_diff_header` (M1); `PAGE_SIZE`.
- Produces: `block::overlay::Overlay` with:
  - `new(base: Arc<BuildFile>, cache_path: &Path) -> io::Result<Overlay>`
  - `size(&self) -> u64`
  - `async read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String>` (block-aligned)
  - `write_at(&self, offset: u64, data: &[u8]) -> Result<(), String>`
  - `write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String>`
  - `export_diff(&self, out: &mut File) -> io::Result<DiffMetadata>`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/block/overlay.rs`:
```rust
#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use bytes::Bytes;
    use uuid::Uuid;

    use super::*;
    use crate::block::build_file::Artifact;
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
        let base = BuildFile::new(template.clone(), Artifact::Rootfs, storage.clone(), dir.path().join("cache"));
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
        let resumed = BuildFile::new(header, Artifact::Rootfs, storage, dir.path().join("resumed"));
        let after_resume = read_all(async |bytes| resumed.read_at(0, bytes).await).await;
        assert_eq!(after_resume, before_pause);
    }

    #[tokio::test]
    async fn rejects_misaligned_reads() {
        let dir = tempfile::tempdir().unwrap();
        let (storage, template) = template(dir.path()).await;
        let base = BuildFile::new(template, Artifact::Rootfs, storage, dir.path().join("cache"));
        let overlay = Overlay::new(Arc::new(base), &dir.path().join("overlay")).unwrap();

        let mut buf = vec![0; 4096];
        assert_eq!(
            overlay.read_at(100, &mut buf).await.unwrap_err(),
            "misaligned read: offset 100 length 4096 block size 4096"
        );
    }
}
```
Add `pub mod overlay;` to `crates/orchestrator/src/block.rs`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator block::overlay`
Expected: FAIL to compile, `cannot find type Overlay`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/block/overlay.rs`:
```rust
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::build_file::BuildFile;
use super::cache::BlockCache;
use crate::storage::header::{DiffMetadata, PAGE_SIZE};

pub struct Overlay {
    base: Arc<BuildFile>,
    cache: BlockCache,
}

impl Overlay {
    pub fn new(base: Arc<BuildFile>, cache_path: &Path) -> io::Result<Self> {
        let cache = BlockCache::create(cache_path, base.size(), PAGE_SIZE)?;
        Ok(Self { base, cache })
    }

    pub fn size(&self) -> u64 {
        self.cache.size()
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

    async fn read_block(&self, offset: u64, block: &mut [u8]) -> Result<(), String> {
        if self.cache.is_present(offset, block.len() as u64) {
            return self
                .cache
                .read_at(offset, block)
                .map_err(|e| format!("read overlay cache at {offset}: {e}"));
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
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator block::`
Expected: PASS, all block tests (15).

- [ ] **Step 5: Commit and push**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p orchestrator
git add crates/orchestrator
git commit -m "feat(orchestrator): copy-on-write overlay with diff export and resume"
git push origin main
```
Expected: CI `test` job is green.
