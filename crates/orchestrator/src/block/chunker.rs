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
