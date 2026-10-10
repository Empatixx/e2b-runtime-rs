use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::MemoryPages;
use crate::block::build_file::{BuildFile, Ready};
use crate::memory::dedup::config::DedupConfig;
use crate::memory::snapshot::inflight::{InflightMemfd, PackedIndex};
use crate::memory::snapshot::{BuildFileBase, GuestMemory, drain_deduped, plan_deduped};
use crate::storage::header::{DiffMetadata, Header};
use crate::telemetry::memfd::record_swap_grace_elapsed;
use crate::telemetry::snapshot_diff::record_snapshot_dedup;

const SWAP_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupedMemfile {
    pub header: Header,
    pub bytes: u64,
}

pub type DedupedFuture = Shared<BoxFuture<'static, Result<DedupedMemfile, String>>>;

#[derive(Clone)]
pub struct ProvisionalMemfile {
    pub build_id: Uuid,
    pub header: Header,
    pub parent: Header,
    pub deduped: DedupedFuture,
    pub memfd: Arc<InflightMemfd>,
    pub swapped: CancellationToken,
    pub created: std::time::Instant,
}

impl ProvisionalMemfile {
    pub fn durable_header(&self) -> BoxFuture<'static, Result<Header, String>> {
        let deduped = self.deduped.clone();
        async move { deduped.await.map(|memfile| memfile.header) }.boxed()
    }

    pub fn mark_swapped(&self) {
        self.swapped.cancel();
    }
}

pub(super) struct InflightDedup {
    pub(super) memfd: Arc<File>,
    pub(super) pages: MemoryPages,
    pub(super) path: PathBuf,
    pub(super) base: Arc<BuildFile>,
    pub(super) dedup: DedupConfig,
    pub(super) parent: Header,
    pub(super) build_id: Uuid,
    pub(super) team_id: String,
    pub(super) pre: DiffMetadata,
    pub(super) free_index: bool,
}

impl InflightDedup {
    pub(super) fn start(self) -> Result<(ProvisionalMemfile, Ready), String> {
        let provisional_id = Uuid::new_v4();
        let header = self.pre.to_provisional_diff_header(&self.parent, provisional_id)?;
        let memfd = Arc::new(InflightMemfd::new(self.memfd.clone(), self.free_index));
        let swapped = CancellationToken::new();
        let (header_tx, header_rx) = oneshot::channel();
        let parent = self.parent.clone();
        let drained = tokio::task::spawn_blocking({
            let memfd = memfd.clone();
            let runtime = tokio::runtime::Handle::current();
            move || self.run(&memfd, runtime, header_tx)
        });
        let ready: Ready = async move { drained.await.map_err(|e| format!("export deduped memfile: {e}"))? }
            .boxed()
            .shared();
        let deduped: DedupedFuture = async move {
            header_rx
                .await
                .unwrap_or_else(|_| Err("export deduped memfile: the dedup stopped".to_owned()))
        }
        .boxed()
        .shared();
        tokio::spawn(release_after_swap(ready.clone(), memfd.clone(), swapped.clone()));
        Ok((
            ProvisionalMemfile {
                build_id: provisional_id,
                header,
                parent,
                deduped,
                memfd,
                swapped,
                created: std::time::Instant::now(),
            },
            ready,
        ))
    }

    fn run(
        self,
        inflight: &InflightMemfd,
        runtime: tokio::runtime::Handle,
        header_tx: oneshot::Sender<Result<DedupedMemfile, String>>,
    ) -> Result<(), String> {
        let memory = GuestMemory::Memfd(self.memfd.clone());
        let base = BuildFileBase {
            file: self.base.clone(),
            runtime,
        };
        let planned = plan_deduped(
            &memory,
            &self.pages.regions,
            &self.pages.dirty,
            &self.pages.empty,
            &base,
            &self.dedup,
        )
        .map_err(|e| format!("export deduped memfile: {e}"));
        let plan = match planned {
            Ok(plan) => plan,
            Err(e) => {
                let _ = header_tx.send(Err(e.clone()));
                return Err(e);
            }
        };
        record_snapshot_dedup(
            &self.team_id,
            "memfile",
            &self.pre,
            Some(&plan.diff),
            self.dedup.best_effort,
        );
        inflight.install_index(PackedIndex::of_pages(&plan.diff.dirty));
        let deduped = self.deduped(&plan.diff);
        if let Ok(deduped) = &deduped {
            inflight.set_durable(deduped.header.clone());
        }
        let _ = header_tx.send(deduped);
        let drained = File::create(&self.path)
            .and_then(|file| {
                let mut out = BufWriter::new(file);
                drain_deduped(&memory, &self.pages.regions, &plan, &mut out).and_then(|()| out.flush())
            })
            .map_err(|e| format!("export deduped memfile: {e}"));
        inflight.mark_drained();
        drained
    }

    fn deduped(&self, diff: &DiffMetadata) -> Result<DedupedMemfile, String> {
        diff.to_diff_header(&self.parent, self.build_id)
            .map(|header| DedupedMemfile {
                header,
                bytes: diff.dirty.len() * diff.block_size,
            })
    }
}

async fn release_after_swap(drained: Ready, memfd: Arc<InflightMemfd>, swapped: CancellationToken) {
    let _ = drained.await;
    tokio::select! {
        () = swapped.cancelled() => {}
        () = tokio::time::sleep(SWAP_GRACE) => {
            record_swap_grace_elapsed();
            tracing::warn!("memfd swap grace elapsed; releasing memfd without a swap signal");
        }
    }
    memfd.release();
}

#[cfg(test)]
mod tests {

    use bytes::Bytes;
    use roaring::RoaringBitmap;

    use super::*;
    use crate::block::build_file::{Artifact, DiffStore};
    use crate::firecracker::memory_info::GuestRegion;
    use crate::memory::snapshot::diff_metadata;
    use crate::storage::backend::Storage;
    use crate::storage::header::{BuildMapping, Metadata, PAGE_SIZE};

    const PAGE: u64 = PAGE_SIZE;

    fn page(value: u8) -> Vec<u8> {
        vec![value; PAGE as usize]
    }

    struct Pipeline {
        _dir: tempfile::TempDir,
        store: Arc<DiffStore>,
        memfile: PathBuf,
        guest: Vec<u8>,
        build_id: Uuid,
        base_id: Uuid,
    }

    async fn pipeline() -> (Pipeline, InflightDedup) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let base_id = Uuid::new_v4();
        let base = [page(1), page(2), page(3), page(4)].concat();
        storage
            .put(&Artifact::Memfile.object(base_id), Bytes::from(base))
            .await
            .unwrap();
        let parent = Header::new(
            Metadata::template(base_id, PAGE, 4 * PAGE),
            vec![BuildMapping {
                offset: 0,
                length: 4 * PAGE,
                build_id: base_id,
                storage_offset: 0,
            }],
        )
        .unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let base_file = Arc::new(BuildFile::new(parent.clone(), Artifact::Memfile, store.clone()));
        let guest = [page(1), page(0xEE), page(0), page(4)].concat();
        let mut memfd = tempfile::tempfile().unwrap();
        memfd.write_all(&guest).unwrap();
        let regions = vec![GuestRegion {
            base_host_virt_addr: 0,
            size: 4 * PAGE,
            offset: 0,
            page_size: PAGE,
        }];
        let dirty: RoaringBitmap = (0..4).collect();
        let pre = diff_metadata(&regions, &dirty, &RoaringBitmap::new()).unwrap();
        let build_id = Uuid::new_v4();
        let memfile = dir.path().join("memfile.diff");
        let inflight = InflightDedup {
            memfd: Arc::new(memfd),
            pages: MemoryPages {
                tracked_writes: false,
                regions,
                dirty,
                empty: RoaringBitmap::new(),
            },
            path: memfile.clone(),
            base: base_file,
            dedup: DedupConfig::default(),
            parent,
            build_id,
            team_id: "team".to_owned(),
            pre,
            free_index: false,
        };
        let pipeline = Pipeline {
            _dir: dir,
            store,
            memfile,
            guest,
            build_id,
            base_id,
        };
        (pipeline, inflight)
    }

    async fn read_all(store: &Arc<DiffStore>, header: &Header) -> Vec<u8> {
        let mut bytes = vec![0; 4 * PAGE as usize];
        BuildFile::new(header.clone(), Artifact::Memfile, store.clone())
            .read_at(0, &mut bytes)
            .await
            .unwrap();
        bytes
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pause_serves_from_the_memfd_until_the_deduped_header_takes_over() {
        let (pipeline, inflight) = pipeline().await;
        let (provisional, ready) = inflight.start().unwrap();
        let store = &pipeline.store;
        store.add_provisional(provisional.build_id, Artifact::Memfile, provisional.memfd.clone());
        store.add_local_serving(
            pipeline.build_id,
            Artifact::Memfile,
            &pipeline.memfile,
            ready.clone(),
            provisional.memfd.clone(),
        );

        let owners: Vec<(Uuid, u64)> = (0..4)
            .map(|index| provisional.header.locate(index * PAGE).unwrap())
            .map(|location| (location.build_id, location.storage_offset))
            .collect();
        assert_eq!(
            owners,
            (0..4)
                .map(|index| (provisional.build_id, index * PAGE))
                .collect::<Vec<_>>()
        );
        assert_eq!(read_all(store, &provisional.header).await, pipeline.guest);

        let deduped = provisional.deduped.clone().await.unwrap();
        ready.await.unwrap();

        let owners: Vec<Uuid> = (0..4)
            .map(|index| deduped.header.locate(index * PAGE).unwrap().build_id)
            .collect();
        assert_eq!(
            owners,
            vec![pipeline.base_id, pipeline.build_id, Uuid::nil(), pipeline.base_id]
        );
        assert_eq!(deduped.bytes, PAGE);
        assert_eq!(std::fs::read(&pipeline.memfile).unwrap(), page(0xEE));
        assert_eq!(read_all(store, &deduped.header).await, pipeline.guest);

        provisional.mark_swapped();
        tokio::time::timeout(Duration::from_secs(5), async {
            while provisional.memfd.is_mapped() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the memfd is released once the swap is signalled");
        assert_eq!(
            read_all(store, &provisional.header).await,
            pipeline.guest,
            "a sandbox still on the provisional header reads the deduped data after the release"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_memfd_is_released_after_the_swap() {
        let memfd = Arc::new(InflightMemfd::new(Arc::new(tempfile::tempfile().unwrap()), false));
        let swapped = CancellationToken::new();
        let released = tokio::spawn(release_after_swap(
            crate::block::build_file::ready_now(),
            memfd.clone(),
            swapped.clone(),
        ));

        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            memfd.is_mapped(),
            "a provisional read can still need it before the swap"
        );
        swapped.cancel();
        released.await.unwrap();

        assert!(!memfd.is_mapped());
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_swap_releases_the_memfd_after_the_grace() {
        let memfd = Arc::new(InflightMemfd::new(Arc::new(tempfile::tempfile().unwrap()), false));

        release_after_swap(
            crate::block::build_file::ready_now(),
            memfd.clone(),
            CancellationToken::new(),
        )
        .await;

        assert!(!memfd.is_mapped());
    }
}
