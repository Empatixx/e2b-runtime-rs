use std::collections::HashMap;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use parking_lot::Mutex;
use tokio::sync::watch;

use super::cache::BlockCache;
use crate::storage::backend::Storage;
use crate::storage::header::PAGE_SIZE;

pub const CHUNK_SIZE: u64 = 4 << 20;
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

const MIN_READ_BATCH: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchState {
    Running,
    Failed(String),
    Done,
}

type Sessions = Arc<Mutex<HashMap<u64, watch::Receiver<FetchState>>>>;

pub struct Chunker {
    source: Arc<Source>,
    sessions: Sessions,
}

pub struct ChunkSlice {
    source: Arc<Source>,
    offset: u64,
    length: usize,
}

impl Deref for ChunkSlice {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.source.cache.slice(self.offset, self.length).unwrap_or_default()
    }
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

    async fn open_with_timeout(
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
        Ok(Self::with_cache(cache, storage, object, timeout))
    }

    pub fn local(storage: Storage, object: &str, path: &Path) -> Result<Self, String> {
        let cache = BlockCache::adopt(path, PAGE_SIZE).map_err(|e| format!("adopt {}: {e}", path.display()))?;
        Ok(Self::with_cache(cache, storage, object, FETCH_TIMEOUT))
    }

    fn with_cache(cache: BlockCache, storage: Storage, object: &str, timeout: Duration) -> Self {
        let source = Source {
            cache: Arc::new(cache),
            storage,
            object: object.to_owned(),
            timeout,
            fetches: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
        };
        Self {
            source: Arc::new(source),
            sessions: Sessions::default(),
        }
    }

    pub fn size(&self) -> u64 {
        self.source.cache.size()
    }

    pub fn fetch_count(&self) -> u64 {
        self.source.fetches.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn fetch_attempts(&self) -> u64 {
        self.source.attempts.load(Ordering::Relaxed)
    }

    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let slice = self.slice(offset, buf.len()).await?;
        buf.copy_from_slice(&slice);
        Ok(())
    }

    pub async fn slice(&self, offset: u64, length: usize) -> Result<ChunkSlice, String> {
        let end = offset + length as u64;
        if end > self.size() {
            return Err(format!(
                "read {offset}..{end} is past the end of {} ({} bytes)",
                self.source.object,
                self.size()
            ));
        }
        for chunk in chunks_covering(offset..end) {
            let chunk_range = self.chunk_range(chunk);
            self.ensure(chunk, offset.max(chunk_range.start)..end.min(chunk_range.end))
                .await?;
        }
        self.source.slice(offset, length)
    }

    async fn ensure(&self, chunk: u64, wanted: Range<u64>) -> Result<(), String> {
        if self.source.is_cached(&wanted) {
            return Ok(());
        }
        let mut progress = self.session(chunk);
        loop {
            let state = progress.borrow_and_update().clone();
            if self.source.is_cached(&wanted) {
                return Ok(());
            }
            match state {
                FetchState::Failed(e) => return Err(e),
                FetchState::Done => return self.cached_after_fetch(&wanted),
                FetchState::Running => {
                    if progress.changed().await.is_err() {
                        return self.cached_after_fetch(&wanted);
                    }
                }
            }
        }
    }

    fn cached_after_fetch(&self, wanted: &Range<u64>) -> Result<(), String> {
        match self.source.is_cached(wanted) {
            true => Ok(()),
            false => Err(format!(
                "fetch of {} {wanted:?} ended without the data",
                self.source.object
            )),
        }
    }

    fn session(&self, chunk: u64) -> watch::Receiver<FetchState> {
        let range = self.chunk_range(chunk);
        let mut sessions = self.sessions.lock();
        if let Some(running) = sessions.get(&chunk) {
            return running.clone();
        }
        let (state, progress) = watch::channel(FetchState::Running);
        if self.source.is_cached(&range) {
            state.send_replace(FetchState::Done);
            return progress;
        }
        sessions.insert(chunk, progress.clone());
        let source = self.source.clone();
        let session = FetchSession::new(chunk, self.sessions.clone(), state, &source.object, &range);
        tokio::spawn(async move {
            let fetched = source.fetch(range, &session.state).await;
            session.finish(fetched);
        });
        progress
    }

    fn chunk_range(&self, chunk: u64) -> Range<u64> {
        let start = chunk * CHUNK_SIZE;
        start..(start + CHUNK_SIZE).min(self.size())
    }
}

struct FetchSession {
    chunk: u64,
    sessions: Sessions,
    state: watch::Sender<FetchState>,
    panicked: String,
    settled: bool,
}

impl FetchSession {
    fn new(chunk: u64, sessions: Sessions, state: watch::Sender<FetchState>, object: &str, range: &Range<u64>) -> Self {
        Self {
            chunk,
            sessions,
            state,
            panicked: format!("fetch of {object} {range:?} panicked"),
            settled: false,
        }
    }

    fn finish(mut self, fetched: Result<(), String>) {
        self.settle(match fetched {
            Ok(()) => FetchState::Done,
            Err(e) => FetchState::Failed(e),
        });
    }

    fn settle(&mut self, state: FetchState) {
        self.settled = true;
        self.sessions.lock().remove(&self.chunk);
        self.state.send_replace(state);
    }
}

impl Drop for FetchSession {
    fn drop(&mut self) {
        if !self.settled {
            self.settle(FetchState::Failed(self.panicked.clone()));
        }
    }
}

impl Source {
    async fn fetch(&self, range: Range<u64>, state: &watch::Sender<FetchState>) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        tokio::time::timeout(self.timeout, self.stream(range.clone(), state))
            .await
            .map_err(|_| format!("fetch {} {range:?}: timed out after {:?}", self.object, self.timeout))??;
        self.fetches.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn stream(&self, range: Range<u64>, state: &watch::Sender<FetchState>) -> Result<(), String> {
        let stream = self.storage.get_stream(&self.object, range.clone()).await?;
        self.stream_from(range, stream, state).await
    }

    async fn stream_from(
        &self,
        range: Range<u64>,
        mut stream: impl futures::Stream<Item = Result<Bytes, String>> + Unpin,
        state: &watch::Sender<FetchState>,
    ) -> Result<(), String> {
        let batch = self.read_batch();
        let mut pending = BytesMut::new();
        let mut filled = range.start;
        while let Some(bytes) = stream.next().await {
            pending.extend_from_slice(&bytes?);
            if pending.len() >= batch {
                let aligned = pending.len() - pending.len() % PAGE_SIZE as usize;
                filled = self.fill(filled, pending.split_to(aligned).freeze()).await?;
                state.send_modify(|_| {});
            }
        }
        let received = filled + pending.len() as u64;
        if received != range.end {
            return Err(format!(
                "get {} {range:?}: got {} of {} bytes",
                self.object,
                received - range.start,
                range.end - range.start
            ));
        }
        self.fill(filled, pending.freeze()).await.map(|_| ())
    }

    fn read_batch(&self) -> usize {
        MIN_READ_BATCH.max(self.cache.block_size() as usize)
    }

    async fn fill(&self, offset: u64, bytes: Bytes) -> Result<u64, String> {
        let end = offset + bytes.len() as u64;
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || cache.fill(offset, &bytes))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("fill cache of {}: {e}", self.object))
            .map(|()| end)
    }

    fn slice(self: &Arc<Self>, offset: u64, length: usize) -> Result<ChunkSlice, String> {
        self.cache
            .slice(offset, length)
            .map_err(|e| format!("read cache of {} at {offset}: {e}", self.object))?;
        Ok(ChunkSlice {
            source: self.clone(),
            offset,
            length,
        })
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
    use object_store::aws::AmazonS3Builder;
    use tempfile::TempDir;

    use super::*;
    use crate::storage::spec::{Provider, StorageSpec};

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
        assert_eq!(chunker.fetch_attempts(), 2);

        let mut cached = vec![0; 100];
        chunker.read_at(100, &mut cached).await.unwrap();
        assert_eq!(cached, data[100..200]);
        assert_eq!(chunker.fetch_attempts(), 2);

        let mut tail = vec![0; 1000];
        chunker.read_at(9 * MIB - 1000, &mut tail).await.unwrap();
        assert_eq!(tail, data[(9 * MIB - 1000) as usize..]);
        assert_eq!(chunker.fetch_attempts(), 3);
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
        assert_eq!(chunker.fetch_attempts(), 1);
    }

    #[tokio::test]
    async fn failed_fetch_is_retried_on_the_next_read() {
        let (dir, storage, data) = setup(4 * MIB).await;
        let chunker = chunker(&dir, storage.clone()).await;
        storage.delete_prefix("build").await.unwrap();

        let mut block = vec![0; 4096];
        let error = chunker.read_at(0, &mut block).await.unwrap_err();
        assert!(error.starts_with("get build/memfile"), "{error}");
        assert_eq!(chunker.fetch_attempts(), 1);

        storage.put(OBJECT, Bytes::from(data.clone())).await.unwrap();
        chunker.read_at(0, &mut block).await.unwrap();
        assert_eq!(block, data[..4096]);
        assert_eq!(chunker.fetch_attempts(), 2);
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
        let spec = StorageSpec {
            provider: Provider::Aws,
            bucket: "silent".to_owned(),
            endpoint,
            use_path_style: true,
            region: "us-east-1".to_owned(),
            ..StorageSpec::local("")
        };
        let credentials = AmazonS3Builder::new()
            .with_access_key_id("key")
            .with_secret_access_key("secret");
        let storage = Storage::s3(&spec, credentials).unwrap();
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

    #[tokio::test]
    async fn streamed_blocks_are_readable_before_the_chunk_ends() {
        let (dir, storage, _) = setup(4 * MIB).await;
        let chunker = chunker(&dir, storage).await;
        let first = Bytes::from(vec![7; MIN_READ_BATCH + 4096]);
        let stream = futures::stream::iter([Ok(first)]).chain(futures::stream::pending());
        let (state, _progress) = watch::channel(FetchState::Running);
        let source = chunker.source.clone();
        let streaming = tokio::spawn(async move { source.stream_from(0..4 * MIB, stream, &state).await });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !chunker.source.is_cached(&(0..4096)) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert!(chunker.source.is_cached(&(0..MIN_READ_BATCH as u64)));
        assert!(!chunker.source.is_cached(&(0..4 * MIB)));
        assert!(!streaming.is_finished());
        streaming.abort();
    }

    #[tokio::test]
    async fn a_short_stream_marks_no_partial_page_as_cached() {
        let (dir, storage, _) = setup(4 * MIB).await;
        let chunker = chunker(&dir, storage).await;
        let short = Bytes::from(vec![7; 4096 + 100]);
        let (state, _progress) = watch::channel(FetchState::Running);

        let streamed = chunker
            .source
            .stream_from(0..4 * MIB, futures::stream::iter([Ok(short)]), &state)
            .await;

        assert!(streamed.unwrap_err().contains("got 4196 of 4194304 bytes"));
        assert!(!chunker.source.is_cached(&(4096..8192)));
    }

    #[tokio::test]
    async fn a_session_dropped_without_finishing_leaves_no_entry_and_fails_its_waiters() {
        let sessions = Sessions::default();
        let (state, progress) = watch::channel(FetchState::Running);
        sessions.lock().insert(3, progress.clone());
        let session = FetchSession::new(3, sessions.clone(), state, OBJECT, &(0..4096));

        drop(session);

        assert!(sessions.lock().is_empty());
        assert_eq!(
            *progress.borrow(),
            FetchState::Failed("fetch of build/memfile 0..4096 panicked".to_owned())
        );
    }
}
