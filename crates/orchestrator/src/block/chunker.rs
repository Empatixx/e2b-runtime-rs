use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;

use super::cache::BlockCache;
use crate::storage::backend::Storage;
use crate::storage::header::PAGE_SIZE;

pub const CHUNK_SIZE: u64 = 4 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

type SharedFetch = Shared<BoxFuture<'static, Result<(), String>>>;

pub struct Chunker {
    source: Arc<Source>,
    in_flight: Mutex<HashMap<u64, SharedFetch>>,
}

struct Source {
    cache: Arc<BlockCache>,
    storage: Storage,
    object: String,
    timeout: Duration,
    fetches: AtomicU64,
    attempts: AtomicU64,
}

impl Chunker {
    pub async fn open(storage: Storage, object: &str, cache_path: &Path) -> Result<Self, String> {
        Self::open_with_timeout(storage, object, cache_path, FETCH_TIMEOUT).await
    }

    pub async fn open_with_timeout(
        storage: Storage,
        object: &str,
        cache_path: &Path,
        timeout: Duration,
    ) -> Result<Self, String> {
        let size = tokio::time::timeout(timeout, storage.size(object))
            .await
            .map_err(|_| format!("head {object}: timed out after {timeout:?}"))??;
        let cache = BlockCache::create(cache_path, size, PAGE_SIZE)
            .map_err(|e| format!("create cache {}: {e}", cache_path.display()))?;
        let source = Source {
            cache: Arc::new(cache),
            storage,
            object: object.to_owned(),
            timeout,
            fetches: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
        };
        Ok(Self {
            source: Arc::new(source),
            in_flight: Mutex::default(),
        })
    }

    pub fn local(storage: Storage, object: &str, path: &Path) -> Result<Self, String> {
        let cache = BlockCache::adopt(path, PAGE_SIZE).map_err(|e| format!("adopt {}: {e}", path.display()))?;
        let source = Source {
            cache: Arc::new(cache),
            storage,
            object: object.to_owned(),
            timeout: FETCH_TIMEOUT,
            fetches: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
        };
        Ok(Self {
            source: Arc::new(source),
            in_flight: Mutex::default(),
        })
    }

    pub fn size(&self) -> u64 {
        self.source.cache.size()
    }

    pub fn fetch_count(&self) -> u64 {
        self.source.fetches.load(Ordering::Relaxed)
    }

    pub fn fetch_attempts(&self) -> u64 {
        self.source.attempts.load(Ordering::Relaxed)
    }

    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let end = offset + buf.len() as u64;
        if end > self.size() {
            return Err(format!(
                "read {offset}..{end} is past the end of {} ({} bytes)",
                self.source.object,
                self.size()
            ));
        }
        for chunk in chunks_covering(offset..end) {
            self.ensure_chunk(chunk).await?;
        }
        let bytes = self.source.clone().read_cached(offset, buf.len()).await?;
        buf.copy_from_slice(&bytes);
        Ok(())
    }

    async fn ensure_chunk(&self, chunk: u64) -> Result<(), String> {
        if self.source.is_cached(&self.chunk_range(chunk)) {
            return Ok(());
        }
        let fetch = self.shared_fetch(chunk);
        let fetched = fetch.clone().await;
        let mut in_flight = self.in_flight.lock();
        if in_flight.get(&chunk).is_some_and(|current| current.ptr_eq(&fetch)) {
            in_flight.remove(&chunk);
        }
        fetched
    }

    fn shared_fetch(&self, chunk: u64) -> SharedFetch {
        let range = self.chunk_range(chunk);
        let mut in_flight = self.in_flight.lock();
        if self.source.is_cached(&range) {
            return futures::future::ready(Ok(())).boxed().shared();
        }
        in_flight
            .entry(chunk)
            .or_insert_with(|| self.source.clone().fetch(range).boxed().shared())
            .clone()
    }

    fn chunk_range(&self, chunk: u64) -> Range<u64> {
        let start = chunk * CHUNK_SIZE;
        start..(start + CHUNK_SIZE).min(self.size())
    }
}

impl Source {
    async fn fetch(self: Arc<Self>, range: Range<u64>) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        let bytes = tokio::time::timeout(self.timeout, self.storage.get_range(&self.object, range.clone()))
            .await
            .map_err(|_| format!("fetch {} {range:?}: timed out after {:?}", self.object, self.timeout))??;
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || cache.fill(range.start, &bytes))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("fill cache of {}: {e}", self.object))?;
        self.fetches.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn read_cached(self: Arc<Self>, offset: u64, length: usize) -> Result<Vec<u8>, String> {
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            let mut bytes = vec![0; length];
            cache.read_at(offset, &mut bytes).map(|()| bytes)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("read cache of {} at {offset}: {e}", self.object))
    }

    fn is_cached(&self, range: &Range<u64>) -> bool {
        self.cache.is_present(range.start, range.end - range.start)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn waiters_share_one_failed_fetch() {
        let (dir, storage, _) = setup(4 * MIB).await;
        let timeout = Duration::from_millis(300);
        let chunker = Chunker::open_with_timeout(storage, OBJECT, &dir.path().join("cache"), timeout)
            .await
            .unwrap();
        let object = dir.path().join("store").join(OBJECT);
        std::fs::remove_file(&object).unwrap();
        nix::unistd::mkfifo(&object, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let chunker = Arc::new(chunker);

        let started = tokio::time::Instant::now();
        let reads = (0..8u64).map(|index| {
            let chunker = chunker.clone();
            tokio::spawn(async move {
                let mut block = vec![0; 4096];
                chunker.read_at(index * 4096, &mut block).await
            })
        });
        let results = futures::future::join_all(reads).await;
        let elapsed = started.elapsed();
        drop(std::fs::OpenOptions::new().write(true).open(&object));

        assert!(results.into_iter().all(|result| result.unwrap().is_err()));
        assert_eq!(chunker.fetch_attempts(), 1);
        assert!(elapsed < timeout * 3, "waiters retried one after another: {elapsed:?}");
    }

    #[tokio::test]
    async fn a_silent_storage_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let storage = Storage::s3()
            .bucket("silent")
            .endpoint(&endpoint)
            .credentials("key", "secret")
            .region("us-east-1")
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();

        let error = Chunker::open_with_timeout(storage, OBJECT, &dir.path().join("cache"), Duration::from_millis(100))
            .await
            .err()
            .unwrap();

        assert_eq!(error, "head build/memfile: timed out after 100ms");
    }

    #[tokio::test]
    async fn a_local_chunker_reads_its_file_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("empty-store")).unwrap();
        let path = dir.path().join("memfile.diff");
        let data = pattern(2 * MIB + 4096);
        std::fs::write(&path, &data).unwrap();

        let chunker = Chunker::local(storage, OBJECT, &path).unwrap();
        let mut buf = vec![0; 8192];
        chunker.read_at(MIB, &mut buf).await.unwrap();

        assert_eq!(buf, data[MIB as usize..MIB as usize + 8192]);
        assert_eq!((chunker.fetch_attempts(), chunker.size()), (0, 2 * MIB + 4096));
    }
}
