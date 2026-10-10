use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt;
use futures::future::BoxFuture;
use roaring::RoaringBitmap;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::fpr_resume::FprResume;
use crate::block::build_file::Ready;
use crate::memory::cow_window::{CowWindow, PackedPageSink, PageSink, PageSource};
use crate::telemetry::server::in_place::record_memory_seal;

pub const ABORTED: &str = "pause aborted before deferred memory export ran";
pub const SEAL_FAILED: &str = "deferred rootfs seal failed";

pub type Begin =
    Box<dyn FnOnce(&RoaringBitmap, Arc<dyn PageSource>, Arc<dyn PageSink>) -> Result<Arc<CowWindow>, String> + Send>;
pub type End = Arc<dyn Fn(Arc<CowWindow>) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct MemfdSource {
    memfd: Arc<File>,
}

impl MemfdSource {
    pub fn new(memfd: Arc<File>) -> Self {
        Self { memfd }
    }
}

impl PageSource for MemfdSource {
    fn read_page(&self, offset: u64, length: u64) -> io::Result<Cow<'_, [u8]>> {
        let mut page = vec![0u8; length as usize];
        self.memfd.read_exact_at(&mut page, offset)?;
        Ok(Cow::Owned(page))
    }
}

pub struct PackedFileSink {
    file: Arc<File>,
}

impl PageSink for PackedFileSink {
    fn write_page(&self, data: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(data, offset)
    }
}

pub struct Deferred {
    pub ready: Ready,
    pub occupancy: Ready,
    pub pending: Pending,
}

pub struct Pending {
    window: Arc<CowWindow>,
    end: End,
    path: PathBuf,
    fpr: Option<FprResume>,
    sealed: oneshot::Sender<Result<(), String>>,
    occupancy: oneshot::Sender<Result<(), String>>,
    team_id: String,
}

pub struct Setup<'a> {
    pub dirty: &'a RoaringBitmap,
    pub page_size: u64,
    pub path: &'a Path,
    pub source: Arc<dyn PageSource>,
    pub begin: Begin,
    pub end: End,
    pub fpr: Option<FprResume>,
    pub team_id: &'a str,
}

pub fn setup(setup: Setup<'_>) -> Result<Deferred, String> {
    let packed_bytes = setup.dirty.len() * setup.page_size;
    let file = File::create(setup.path)
        .and_then(|file| file.set_len(packed_bytes).map(|()| file))
        .map_err(|e| format!("creating memory export cache: {e}"))?;
    let sink = PackedPageSink::new(
        setup.dirty.clone(),
        setup.page_size,
        Arc::new(PackedFileSink { file: Arc::new(file) }),
    );
    let window = (setup.begin)(setup.dirty, setup.source, Arc::new(sink)).inspect_err(|_| {
        let _ = std::fs::remove_file(setup.path);
    })?;
    let (sealed, sealed_rx) = oneshot::channel();
    let (occupancy, occupancy_rx) = oneshot::channel();
    Ok(Deferred {
        ready: ready_from(sealed_rx),
        occupancy: ready_from(occupancy_rx),
        pending: Pending {
            window,
            end: setup.end,
            path: setup.path.to_path_buf(),
            fpr: setup.fpr,
            sealed,
            occupancy,
            team_id: setup.team_id.to_owned(),
        },
    })
}

fn ready_from(receiver: oneshot::Receiver<Result<(), String>>) -> Ready {
    async move { receiver.await.unwrap_or_else(|_| Err(ABORTED.to_owned())) }
        .boxed()
        .shared()
}

impl Pending {
    pub fn start(self) {
        tokio::spawn(self.run());
    }

    pub async fn abort(self) {
        crate::telemetry::uffd::record_cow_cancel("pause_abort");
        self.window.cancel_and_drain(ABORTED.to_owned());
        (self.end)(self.window.clone()).await;
        if let Some(fpr) = &self.fpr {
            fpr.run().await;
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = self.sealed.send(Err(ABORTED.to_owned()));
        let _ = self.occupancy.send(Ok(()));
    }

    pub async fn run(self) {
        let started = Instant::now();
        let swept = self.window.sweep(&CancellationToken::new()).await;
        (self.end)(self.window.clone()).await;
        let outcome = match swept {
            Ok(()) => self.window.cancel_cause().map_or(Ok(()), Err),
            Err(error) => Err(error),
        };
        record_memory_seal(outcome.is_ok(), started.elapsed(), &self.team_id);
        if let Some(fpr) = &self.fpr {
            fpr.run().await;
        }
        match outcome {
            Ok(()) => {
                let _ = self.sealed.send(Ok(()));
            }
            Err(error) => {
                tracing::error!(error = %error, "deferred memory export failed");
                self.window.cancel_and_drain(error.clone());
                let _ = std::fs::remove_file(&self.path);
                let _ = self.sealed.send(Err(format!("{SEAL_FAILED}: {error}")));
            }
        }
        let _ = self.occupancy.send(Ok(()));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::memory::cow_window::CANCELED;

    const PAGE: u64 = 4096;

    struct Zeros;

    impl PageSource for Zeros {
        fn read_page(&self, _offset: u64, length: u64) -> io::Result<Cow<'_, [u8]>> {
            Ok(Cow::Owned(vec![0; length as usize]))
        }
    }

    struct Rig {
        ended: Arc<AtomicBool>,
        end: End,
    }

    fn rig() -> Rig {
        let ended = Arc::new(AtomicBool::new(false));
        let seen = ended.clone();
        Rig {
            ended,
            end: Arc::new(move |_| {
                seen.store(true, Ordering::SeqCst);
                Box::pin(async {})
            }),
        }
    }

    fn deferred(dir: &Path, rig: &Rig, dirty: &RoaringBitmap) -> Result<Deferred, String> {
        let begin: Begin =
            Box::new(|dirty, source, sink| Ok(Arc::new(CowWindow::new(dirty.clone(), PAGE, source, sink, None))));
        setup(Setup {
            dirty,
            page_size: PAGE,
            path: &dir.join("memfile.diff"),
            source: Arc::new(Zeros),
            begin,
            end: rig.end.clone(),
            fpr: None,
            team_id: "team",
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_swept_window_resolves_the_artifact_and_the_occupancy() {
        let dir = tempfile::tempdir().unwrap();
        let rig = rig();
        let dirty: RoaringBitmap = [0, 2].into_iter().collect();
        let deferred = deferred(dir.path(), &rig, &dirty).unwrap();

        deferred.pending.start();

        deferred.ready.await.unwrap();
        deferred.occupancy.await.unwrap();
        assert!(rig.ended.load(Ordering::SeqCst), "the runner must end the export");
        assert_eq!(
            std::fs::metadata(dir.path().join("memfile.diff")).unwrap().len(),
            2 * PAGE
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_canceled_window_loses_only_the_artifact_and_spares_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let rig = rig();
        let dirty: RoaringBitmap = [0, 1].into_iter().collect();
        let deferred = deferred(dir.path(), &rig, &dirty).unwrap();
        deferred
            .pending
            .window
            .cancel("uffd REMOVE overlaps uncaptured CoW-window pages".to_owned());

        deferred.pending.run().await;

        let error = deferred.ready.await.unwrap_err();
        assert!(error.starts_with(SEAL_FAILED), "{error}");
        assert!(error.contains(CANCELED), "{error}");
        assert!(
            rig.ended.load(Ordering::SeqCst),
            "the runner must end the export on the failure path"
        );
        assert_eq!(
            deferred.occupancy.await,
            Ok(()),
            "a failed capture must resolve the seal signal success, not latch"
        );
        assert!(!dir.path().join("memfile.diff").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn aborting_before_the_start_settles_everything_without_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let rig = rig();
        let dirty: RoaringBitmap = [0, 2].into_iter().collect();
        let deferred = deferred(dir.path(), &rig, &dirty).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(10), deferred.pending.abort())
            .await
            .expect("the abort cleanup deadlocked");

        assert_eq!(deferred.ready.await.unwrap_err(), ABORTED);
        assert!(
            rig.ended.load(Ordering::SeqCst),
            "the abort cleanup must end the export"
        );
        assert_eq!(deferred.occupancy.await, Ok(()));
    }

    #[tokio::test]
    async fn the_artifact_is_not_ready_while_the_export_is_unsettled() {
        let dir = tempfile::tempdir().unwrap();
        let rig = rig();
        let dirty: RoaringBitmap = [1].into_iter().collect();
        let deferred = deferred(dir.path(), &rig, &dirty).unwrap();

        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), deferred.ready.clone()).await;

        assert!(
            waited.is_err(),
            "the seal gate must block while the export is unsettled"
        );
        deferred.pending.abort().await;
    }

    #[tokio::test]
    async fn a_failed_begin_leaves_no_cache_behind() {
        let dir = tempfile::tempdir().unwrap();
        let dirty: RoaringBitmap = [1].into_iter().collect();
        let begin: Begin = Box::new(|_, _, _| Err("arm write-protect: nope".to_owned()));
        let path = dir.path().join("memfile.diff");

        let failed = setup(Setup {
            dirty: &dirty,
            page_size: PAGE,
            path: &path,
            source: Arc::new(Zeros),
            begin,
            end: rig().end,
            fpr: None,
            team_id: "team",
        });

        assert_eq!(failed.err().as_deref(), Some("arm write-protect: nope"));
        assert!(!path.exists());
    }

    #[test]
    fn the_packed_file_sink_and_memfd_source_move_whole_pages() {
        let dir = tempfile::tempdir().unwrap();
        let source_file = dir.path().join("memfd");
        std::fs::write(&source_file, vec![7u8; 2 * PAGE as usize]).unwrap();
        let source = MemfdSource::new(Arc::new(File::open(&source_file).unwrap()));
        let target = File::create(dir.path().join("out")).unwrap();
        target.set_len(PAGE).unwrap();
        let sink = PackedFileSink { file: Arc::new(target) };

        let page = source.read_page(PAGE, PAGE).unwrap();
        sink.write_page(&page, 0).unwrap();

        assert_eq!(std::fs::read(dir.path().join("out")).unwrap(), vec![7u8; PAGE as usize]);
    }
}
