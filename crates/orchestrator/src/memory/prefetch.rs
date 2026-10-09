pub mod flags;
pub mod last_cycle;
pub mod plan;
pub mod source;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use futures::StreamExt;
use tokio::sync::mpsc;

use crate::block::build_file::BuildFile;
use crate::template::metadata::MemoryPrefetch;

pub const FETCH_WORKERS: usize = 16;
pub const COPY_WORKERS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefaulted {
    Installed,
    Present,
    Skipped,
    Deferred,
    Closed,
}

pub trait Installer: Send + Sync + 'static {
    fn page_size(&self) -> u64;
    fn prefault(&self, offset: u64, data: &[u8]) -> Result<Prefaulted, String>;
}

#[derive(Debug, Default)]
pub struct PrefetchStats {
    pub fetched: AtomicU64,
    pub installed: AtomicU64,
    pub skipped: AtomicU64,
}

impl PrefetchStats {
    pub fn installed(&self) -> u64 {
        self.installed.load(Ordering::Relaxed)
    }
}

pub async fn prefetch<I: Installer>(
    mapping: MemoryPrefetch,
    memfile: Arc<BuildFile>,
    installer: impl Future<Output = Option<Arc<I>>>,
    stats: Arc<PrefetchStats>,
) -> Result<(), String> {
    let block_size = u64::try_from(mapping.block_size).map_err(|_| "negative prefetch block size".to_owned())?;
    let (warmed, mut ready) = mpsc::unbounded_channel();
    let warming = tokio::spawn(warm(
        mapping.indices,
        block_size,
        memfile.clone(),
        warmed,
        stats.clone(),
    ));
    let Some(installer) = installer.await else {
        warming.abort();
        return Ok(());
    };
    if installer.page_size() != block_size {
        warming.abort();
        return Err(format!(
            "prefetch block size {block_size} differs from the page size {}",
            installer.page_size()
        ));
    }
    let closed = Arc::new(AtomicBool::new(false));
    let fetching = warming.abort_handle();
    futures::stream::poll_fn(|context| ready.poll_recv(context))
        .take_while(|_| std::future::ready(!closed.load(Ordering::Relaxed)))
        .for_each_concurrent(COPY_WORKERS, |offset| {
            let installer = installer.clone();
            let memfile = memfile.clone();
            let stats = stats.clone();
            let closed = closed.clone();
            let fetching = fetching.clone();
            async move {
                let mut data = vec![0; block_size as usize];
                match memfile.read_at(offset, &mut data).await {
                    Ok(()) => {
                        if install_blocking(installer, offset, data, stats).await == Flow::Stop {
                            closed.store(true, Ordering::Relaxed);
                            fetching.abort();
                        }
                    }
                    Err(_) => {
                        stats.skipped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .await;
    Ok(())
}

fn fetched_blocks(
    indices: Vec<u64>,
    block_size: u64,
    memfile: Arc<BuildFile>,
) -> impl futures::Stream<Item = Result<u64, String>> {
    let size = memfile.size();
    futures::stream::iter(indices)
        .filter_map(move |index| {
            std::future::ready(
                index
                    .checked_mul(block_size)
                    .filter(|offset| offset.checked_add(block_size).is_some_and(|end| end <= size)),
            )
        })
        .map(move |offset| {
            let memfile = memfile.clone();
            async move {
                let mut data = vec![0; block_size as usize];
                memfile.read_at(offset, &mut data).await.map(|()| offset)
            }
        })
        .buffered(FETCH_WORKERS)
}

pub async fn fetch_only(mapping: MemoryPrefetch, memfile: Arc<BuildFile>, stats: Arc<PrefetchStats>) {
    let Ok(block_size) = u64::try_from(mapping.block_size) else {
        return;
    };
    let reads = fetched_blocks(mapping.indices, block_size, memfile);
    futures::pin_mut!(reads);
    while let Some(read) = reads.next().await {
        record_fetch(&read, &stats);
    }
}

fn record_fetch(read: &Result<u64, String>, stats: &PrefetchStats) {
    match read {
        Ok(_) => stats.fetched.fetch_add(1, Ordering::Relaxed),
        Err(e) => {
            tracing::warn!(error = %e, "prefetch read failed");
            stats.skipped.fetch_add(1, Ordering::Relaxed)
        }
    };
}

async fn warm(
    indices: Vec<u64>,
    block_size: u64,
    memfile: Arc<BuildFile>,
    warmed: mpsc::UnboundedSender<u64>,
    stats: Arc<PrefetchStats>,
) {
    let reads = fetched_blocks(indices, block_size, memfile);
    futures::pin_mut!(reads);
    while let Some(read) = reads.next().await {
        record_fetch(&read, &stats);
        if let Ok(offset) = read
            && warmed.send(offset).is_err()
        {
            return;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

async fn install_blocking<I: Installer>(
    installer: Arc<I>,
    offset: u64,
    data: Vec<u8>,
    stats: Arc<PrefetchStats>,
) -> Flow {
    tokio::task::spawn_blocking(move || install(&*installer, offset, &data, &stats))
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "prefault task failed"))
        .unwrap_or(Flow::Continue)
}

fn install(installer: &dyn Installer, offset: u64, data: &[u8], stats: &PrefetchStats) -> Flow {
    match installer.prefault(offset, data) {
        Ok(Prefaulted::Installed) => stats.installed.fetch_add(1, Ordering::Relaxed),
        Ok(Prefaulted::Closed) => return Flow::Stop,
        Ok(_) => stats.skipped.fetch_add(1, Ordering::Relaxed),
        Err(e) => {
            tracing::debug!(offset, error = %e, "prefault failed");
            stats.skipped.fetch_add(1, Ordering::Relaxed)
        }
    };
    Flow::Continue
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use bytes::Bytes;
    use parking_lot::Mutex;
    use uuid::Uuid;

    use super::*;
    use crate::block::build_file::{Artifact, DiffStore};
    use crate::storage::backend::Storage;
    use crate::storage::header::{Header, Metadata};

    const PAGE: u64 = 4096;

    #[derive(Default)]
    struct FakeGuest {
        installed: Mutex<Vec<(u64, u8)>>,
        present: Vec<u64>,
    }

    impl Installer for FakeGuest {
        fn page_size(&self) -> u64 {
            PAGE
        }

        fn prefault(&self, offset: u64, data: &[u8]) -> Result<Prefaulted, String> {
            if self.present.contains(&offset) {
                return Ok(Prefaulted::Skipped);
            }
            self.installed.lock().push((offset, data[0]));
            Ok(Prefaulted::Installed)
        }
    }

    #[derive(Default)]
    struct SlowGuest {
        running: AtomicUsize,
        peak: AtomicUsize,
    }

    impl Installer for SlowGuest {
        fn page_size(&self) -> u64 {
            PAGE
        }

        fn prefault(&self, _offset: u64, _data: &[u8]) -> Result<Prefaulted, String> {
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(running, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
            self.running.fetch_sub(1, Ordering::SeqCst);
            Ok(Prefaulted::Installed)
        }
    }

    #[derive(Default)]
    struct ClosedGuest {
        attempts: AtomicUsize,
    }

    impl Installer for ClosedGuest {
        fn page_size(&self) -> u64 {
            PAGE
        }

        fn prefault(&self, _offset: u64, _data: &[u8]) -> Result<Prefaulted, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            Ok(Prefaulted::Closed)
        }
    }

    async fn memfile(dir: &tempfile::TempDir, pages: u64) -> Arc<BuildFile> {
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let build_id = Uuid::new_v4();
        let content: Vec<u8> = (0..pages).flat_map(|page| vec![page as u8; PAGE as usize]).collect();
        storage
            .put(&Artifact::Memfile.object(build_id), Bytes::from(content))
            .await
            .unwrap();
        let header = Header::new(Metadata::template(build_id, PAGE, pages * PAGE), Vec::new()).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        Arc::new(BuildFile::new(header, Artifact::Memfile, store))
    }

    fn mapping(indices: Vec<u64>, block_size: i64) -> MemoryPrefetch {
        MemoryPrefetch {
            access_types: vec!["r".to_owned(); indices.len()],
            indices,
            block_size,
        }
    }

    #[tokio::test]
    async fn installs_the_traced_pages_and_skips_present_or_outside_ones() {
        let dir = tempfile::tempdir().unwrap();
        let guest = Arc::new(FakeGuest {
            present: vec![2 * PAGE],
            ..FakeGuest::default()
        });
        let stats = Arc::new(PrefetchStats::default());

        prefetch(
            mapping(vec![3, 2, 0, 99, u64::MAX], PAGE as i64),
            memfile(&dir, 4).await,
            std::future::ready(Some(guest.clone())),
            stats.clone(),
        )
        .await
        .unwrap();

        let mut installed = guest.installed.lock().clone();
        installed.sort();
        assert_eq!(installed, vec![(0, 0), (3 * PAGE, 3)]);
        assert_eq!(stats.installed(), 2);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_trace_with_another_block_size_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let error = prefetch(
            mapping(vec![0], 2 << 20),
            memfile(&dir, 1).await,
            std::future::ready(Some(Arc::new(FakeGuest::default()))),
            Arc::default(),
        )
        .await
        .unwrap_err();

        assert_eq!(error, "prefetch block size 2097152 differs from the page size 4096");
    }

    #[tokio::test]
    async fn copies_run_in_parallel_on_blocking_workers() {
        let dir = tempfile::tempdir().unwrap();
        let guest = Arc::new(SlowGuest::default());

        prefetch(
            mapping((0..16).collect(), PAGE as i64),
            memfile(&dir, 16).await,
            std::future::ready(Some(guest.clone())),
            Arc::default(),
        )
        .await
        .unwrap();

        assert!(
            guest.peak.load(Ordering::SeqCst) >= 2,
            "the copies ran one after another"
        );
    }

    #[tokio::test]
    async fn a_closed_guest_memory_ends_the_prefetch() {
        let dir = tempfile::tempdir().unwrap();
        let guest = Arc::new(ClosedGuest::default());
        let stats = Arc::new(PrefetchStats::default());

        prefetch(
            mapping((0..200).collect(), PAGE as i64),
            memfile(&dir, 200).await,
            std::future::ready(Some(guest.clone())),
            stats.clone(),
        )
        .await
        .unwrap();

        assert!(guest.attempts.load(Ordering::SeqCst) <= 2 * COPY_WORKERS);
        assert_eq!(stats.installed(), 0);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn fetch_only_reads_the_traced_blocks_without_an_installer() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(PrefetchStats::default());

        fetch_only(
            mapping(vec![3, 2, 0, 99, u64::MAX], PAGE as i64),
            memfile(&dir, 4).await,
            stats.clone(),
        )
        .await;

        assert_eq!(stats.fetched.load(Ordering::Relaxed), 3);
        assert_eq!(stats.installed(), 0);
        assert_eq!(stats.skipped.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn fetch_only_ignores_a_negative_block_size() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(PrefetchStats::default());

        fetch_only(mapping(vec![0], -1), memfile(&dir, 1).await, stats.clone()).await;

        assert_eq!(stats.fetched.load(Ordering::Relaxed), 0);
    }
}
