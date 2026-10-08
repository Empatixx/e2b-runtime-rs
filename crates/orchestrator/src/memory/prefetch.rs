use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    let (pages, mut fetched) = mpsc::channel(mapping.indices.len().max(1));
    let fetching = tokio::spawn(fetch(mapping.indices, block_size, memfile, pages, stats.clone()));
    let Some(installer) = installer.await else {
        fetching.abort();
        return Ok(());
    };
    if installer.page_size() != block_size {
        fetching.abort();
        return Err(format!(
            "prefetch block size {block_size} differs from the page size {}",
            installer.page_size()
        ));
    }
    let copies = futures::stream::poll_fn(|context| fetched.poll_recv(context));
    copies
        .for_each_concurrent(COPY_WORKERS, |(offset, data)| {
            let installer = installer.clone();
            let stats = stats.clone();
            async move { install(&*installer, offset, &data, &stats) }
        })
        .await;
    Ok(())
}

async fn fetch(
    indices: Vec<u64>,
    block_size: u64,
    memfile: Arc<BuildFile>,
    pages: mpsc::Sender<(u64, Vec<u8>)>,
    stats: Arc<PrefetchStats>,
) {
    let size = memfile.size();
    let reads = futures::stream::iter(indices)
        .map(|index| index * block_size)
        .filter(|offset| std::future::ready(offset + block_size <= size))
        .map(|offset| {
            let memfile = memfile.clone();
            async move {
                let mut data = vec![0; block_size as usize];
                memfile.read_at(offset, &mut data).await.map(|()| (offset, data))
            }
        })
        .buffered(FETCH_WORKERS);
    futures::pin_mut!(reads);
    while let Some(read) = reads.next().await {
        match read {
            Ok(page) => {
                stats.fetched.fetch_add(1, Ordering::Relaxed);
                if pages.send(page).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "prefetch read failed");
                stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn install(installer: &dyn Installer, offset: u64, data: &[u8], stats: &PrefetchStats) {
    match installer.prefault(offset, data) {
        Ok(Prefaulted::Installed) => stats.installed.fetch_add(1, Ordering::Relaxed),
        Ok(_) => stats.skipped.fetch_add(1, Ordering::Relaxed),
        Err(e) => {
            tracing::debug!(offset, error = %e, "prefault failed");
            stats.skipped.fetch_add(1, Ordering::Relaxed)
        }
    };
}

#[cfg(test)]
mod tests {
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
            mapping(vec![3, 2, 0, 99], PAGE as i64),
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
}
