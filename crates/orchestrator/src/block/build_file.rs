use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use tokio::sync::OnceCell;
use tokio::time::Instant;
use uuid::Uuid;

use super::chunker::Chunker;
use crate::storage::backend::Storage;
use crate::storage::header::Header;
use crate::storage::paths::BuildPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DiffKey {
    build_id: Uuid,
    artifact: Artifact,
}

pub const DIFF_EXPIRATION: Duration = Duration::from_secs(25 * 60 * 60);
pub const EVICTION_DELAY: Duration = Duration::from_secs(60);
const EVICTION_TICK: Duration = Duration::from_secs(1);

type ChunkerCell = Arc<OnceCell<Arc<Chunker>>>;

pub struct DiffStore {
    storage: Storage,
    cache_dir: PathBuf,
    entries: Mutex<HashMap<DiffKey, Entry>>,
}

pub type Ready = Shared<BoxFuture<'static, Result<(), String>>>;

pub fn ready_now() -> Ready {
    futures::future::ready(Ok(())).boxed().shared()
}

#[derive(Clone)]
struct Pending {
    path: PathBuf,
    ready: Ready,
}

struct Entry {
    cell: ChunkerCell,
    expires: Instant,
    pinned: bool,
    doomed: Option<Instant>,
    pending: Option<Pending>,
}

impl Entry {
    fn new(cell: ChunkerCell, pinned: bool) -> Self {
        Self {
            cell,
            expires: Instant::now() + DIFF_EXPIRATION,
            pinned,
            doomed: None,
            pending: None,
        }
    }

    fn touch(&mut self) -> ChunkerCell {
        self.expires = Instant::now() + DIFF_EXPIRATION;
        self.doomed = None;
        self.cell.clone()
    }

    fn is_gone(&self, now: Instant) -> bool {
        !self.pinned && (self.expires <= now || self.doomed.is_some_and(|doomed| doomed <= now))
    }
}

impl DiffStore {
    pub fn new(storage: Storage, cache_dir: PathBuf) -> Self {
        Self {
            storage,
            cache_dir,
            entries: Mutex::default(),
        }
    }

    pub async fn chunker(&self, build_id: Uuid, artifact: Artifact) -> Result<Arc<Chunker>, String> {
        let (cell, pending) = self.cell(DiffKey { build_id, artifact });
        cell.get_or_try_init(|| async {
            match pending {
                Some(pending) => {
                    pending.ready.await?;
                    Chunker::local(self.storage.clone(), &artifact.object(build_id), &pending.path).map(Arc::new)
                }
                None => self.open(build_id, artifact).await,
            }
        })
        .await
        .cloned()
    }

    pub fn add_local_when(&self, build_id: Uuid, artifact: Artifact, path: &Path, ready: Ready) {
        let mut entry = Entry::new(Arc::default(), true);
        entry.pending = Some(Pending {
            path: path.to_path_buf(),
            ready,
        });
        self.entries.lock().insert(DiffKey { build_id, artifact }, entry);
    }

    pub fn add_local(&self, build_id: Uuid, artifact: Artifact, path: &Path) -> Result<(), String> {
        let chunker = Chunker::local(self.storage.clone(), &artifact.object(build_id), path)?;
        let cell = Arc::new(OnceCell::new_with(Some(Arc::new(chunker))));
        self.entries
            .lock()
            .insert(DiffKey { build_id, artifact }, Entry::new(cell, true));
        Ok(())
    }

    pub fn unpin(&self, build_id: Uuid) {
        self.entries
            .lock()
            .iter_mut()
            .filter(|(key, _)| key.build_id == build_id)
            .for_each(|(_, entry)| entry.pinned = false);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    pub fn evict(&self, over_limit: bool) {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        if over_limit {
            let next = entries
                .values_mut()
                .filter(|entry| !entry.pinned && entry.doomed.is_none())
                .min_by_key(|entry| entry.expires);
            if let Some(entry) = next {
                entry.doomed = Some(now + EVICTION_DELAY);
            }
        }
        entries.retain(|_, entry| !entry.is_gone(now));
    }

    pub async fn run_eviction(self: Arc<Self>, max_usage_percent: f64) {
        let mut ticks = tokio::time::interval(EVICTION_TICK);
        loop {
            ticks.tick().await;
            let over_limit = disk_usage_percent(&self.cache_dir).is_some_and(|usage| usage > max_usage_percent);
            self.evict(over_limit);
        }
    }

    fn cell(&self, key: DiffKey) -> (ChunkerCell, Option<Pending>) {
        let mut entries = self.entries.lock();
        let entry = entries.entry(key).or_insert_with(|| Entry::new(Arc::default(), false));
        (entry.touch(), entry.pending.clone())
    }

    async fn open(&self, build_id: Uuid, artifact: Artifact) -> Result<Arc<Chunker>, String> {
        let cache_path = self
            .cache_dir
            .join(format!("{build_id}-{}-{}", artifact.name(), Uuid::new_v4()));
        Chunker::open(self.storage.clone(), &artifact.object(build_id), &cache_path)
            .await
            .map(Arc::new)
    }
}

fn disk_usage_percent(path: &Path) -> Option<f64> {
    let stats = nix::sys::statvfs::statvfs(path).ok()?;
    let total = stats.blocks() as f64;
    (total > 0.0).then(|| (total - stats.blocks_free() as f64) * 100.0 / total)
}

pub struct BuildFile {
    header: Header,
    artifact: Artifact,
    store: Arc<DiffStore>,
}

impl BuildFile {
    pub fn new(header: Header, artifact: Artifact, store: Arc<DiffStore>) -> Self {
        Self {
            header,
            artifact,
            store,
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
                self.store
                    .chunker(location.build_id, self.artifact)
                    .await?
                    .read_at(location.storage_offset, target)
                    .await?;
            }
            done += length;
        }
        Ok(())
    }
}

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
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let file = BuildFile::new(header, Artifact::Rootfs, store);

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

    #[tokio::test]
    async fn build_files_share_one_chunker_per_build() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(1)), Bytes::from(vec![1; 4096]))
            .await
            .unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let header = Header::new(Metadata::template(id(1), 4096, 4096), Vec::new()).unwrap();
        let first = BuildFile::new(header.clone(), Artifact::Rootfs, store.clone());
        let second = BuildFile::new(header, Artifact::Rootfs, store.clone());

        let mut block = vec![0; 4096];
        first.read_at(0, &mut block).await.unwrap();
        second.read_at(0, &mut block).await.unwrap();
        assert_eq!(block, vec![1; 4096]);

        let chunker = store.chunker(id(1), Artifact::Rootfs).await.unwrap();
        assert_eq!(chunker.fetch_count(), 1);
    }

    #[tokio::test]
    async fn a_failed_open_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let store = Arc::new(DiffStore::new(storage.clone(), dir.path().join("cache")));
        assert!(store.chunker(id(1), Artifact::Memfile).await.is_err());

        storage
            .put(&Artifact::Memfile.object(id(1)), Bytes::from(vec![3; 4096]))
            .await
            .unwrap();
        assert_eq!(store.chunker(id(1), Artifact::Memfile).await.unwrap().size(), 4096);
    }

    #[tokio::test]
    async fn a_local_diff_is_read_without_storage() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        storage
            .put(&Artifact::Memfile.object(id(1)), Bytes::from(vec![1; 2 * 4096]))
            .await
            .unwrap();
        let diff = dir.path().join("memfile.diff");
        std::fs::write(&diff, vec![2; 4096]).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        store.add_local(id(2), Artifact::Memfile, &diff).unwrap();
        let metadata = Metadata::template(id(1), 4096, 2 * 4096).next_generation(id(2));
        let header = Header::new(metadata, vec![mapping(0, id(1), 0), mapping(4096, id(2), 0)]).unwrap();

        let mut whole = vec![0; 2 * 4096];
        BuildFile::new(header, Artifact::Memfile, store.clone())
            .read_at(0, &mut whole)
            .await
            .unwrap();

        assert_eq!(whole, [vec![1; 4096], vec![2; 4096]].concat());
        assert_eq!(
            store.chunker(id(2), Artifact::Memfile).await.unwrap().fetch_attempts(),
            0
        );
    }

    async fn store_with_one_diff(dir: &tempfile::TempDir) -> Arc<DiffStore> {
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(1)), Bytes::from(vec![1; 4096]))
            .await
            .unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        store.chunker(id(1), Artifact::Rootfs).await.unwrap();
        store
    }

    fn cache_files(dir: &tempfile::TempDir) -> usize {
        std::fs::read_dir(dir.path().join("cache")).map_or(0, |files| files.count())
    }

    #[tokio::test(start_paused = true)]
    async fn unused_diffs_expire_and_their_cache_files_go() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_one_diff(&dir).await;
        assert_eq!(cache_files(&dir), 1);

        tokio::time::advance(DIFF_EXPIRATION).await;
        store.evict(false);

        assert!(store.is_empty());
        assert_eq!(cache_files(&dir), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pinned_local_diff_survives_until_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_one_diff(&dir).await;
        let diff = dir.path().join("local.diff");
        std::fs::write(&diff, vec![2; 4096]).unwrap();
        store.add_local(id(2), Artifact::Rootfs, &diff).unwrap();

        tokio::time::advance(DIFF_EXPIRATION).await;
        store.evict(true);
        tokio::time::advance(EVICTION_DELAY).await;
        store.evict(true);
        assert_eq!(store.len(), 1);

        store.unpin(id(2));
        store.evict(false);
        assert!(store.is_empty());
        assert!(!diff.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn disk_pressure_dooms_the_soonest_expiring_diff_unless_it_is_used_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with_one_diff(&dir).await;

        store.evict(true);
        tokio::time::advance(EVICTION_DELAY / 2).await;
        store.chunker(id(1), Artifact::Rootfs).await.unwrap();
        tokio::time::advance(EVICTION_DELAY).await;
        store.evict(false);
        assert_eq!(store.len(), 1);

        store.evict(true);
        tokio::time::advance(EVICTION_DELAY).await;
        store.evict(false);
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn a_pending_local_diff_is_read_only_after_it_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let diff = dir.path().join("memfile.diff");
        let (done, ready) = tokio::sync::oneshot::channel::<()>();
        let ready: Ready = async move { ready.await.map_err(|e| e.to_string()) }.boxed().shared();
        store.add_local_when(id(4), Artifact::Memfile, &diff, ready);

        let reading = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .chunker(id(4), Artifact::Memfile)
                    .await
                    .map(|chunker| chunker.size())
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!reading.is_finished());
        std::fs::write(&diff, vec![3; 4096]).unwrap();
        done.send(()).unwrap();

        assert_eq!(reading.await.unwrap(), Ok(4096));
    }
}
