mod bitmap;
mod io;
mod packed_sink;
mod runs;
#[cfg(test)]
mod testing;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use parking_lot::Mutex;
use roaring::RoaringBitmap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use self::bitmap::AtomicBitmap;
pub use self::io::{PageSink, PageSource};
pub use self::packed_sink::PackedPageSink;
pub use self::runs::{WriteProtectRun, coalesce_write_protect_runs};
use super::page_tracker::PageTracker;
use crate::telemetry::uffd::{CaptureSource, record_cow_cancel, record_cow_capture};

pub const CANCELED: &str = "memory CoW window canceled";

type Completion = Option<Result<(), String>>;

pub struct CowWindow {
    pages: RoaringBitmap,
    page_size: u64,
    source: Arc<dyn PageSource>,
    sink: Arc<dyn PageSink>,
    tracker: Option<Arc<PageTracker>>,
    copied: AtomicBitmap,
    claims: Mutex<HashMap<u32, watch::Receiver<bool>>>,
    remaining: AtomicI64,
    canceled: AtomicBool,
    cancel_cause: OnceLock<String>,
    done: watch::Sender<Completion>,
}

impl CowWindow {
    pub fn new(
        pages: RoaringBitmap,
        page_size: u64,
        source: Arc<dyn PageSource>,
        sink: Arc<dyn PageSink>,
        tracker: Option<Arc<PageTracker>>,
    ) -> Self {
        let highest = pages.max().unwrap_or(0);
        let window = Self {
            copied: AtomicBitmap::with_bits(highest + 1),
            remaining: AtomicI64::new(pages.len() as i64),
            pages,
            page_size,
            source,
            sink,
            tracker,
            claims: Mutex::default(),
            canceled: AtomicBool::new(false),
            cancel_cause: OnceLock::new(),
            done: watch::channel(None).0,
        };
        if window.pages.is_empty() {
            window.settle(Ok(()));
        }
        window
    }

    pub async fn ensure_copied(&self, page: u32) -> bool {
        self.capture(page, CaptureSource::Fault).await
    }

    async fn capture(&self, page: u32, via: CaptureSource) -> bool {
        if !self.pages.contains(page) || self.canceled.load(Ordering::Acquire) {
            return false;
        }
        if self.copied.get(page) {
            return true;
        }
        match self.claim(page) {
            Claim::Settled => !self.canceled.load(Ordering::Acquire),
            Claim::Waiting(mut finished) => {
                let _ = finished.wait_for(|finished| *finished).await;
                true
            }
            Claim::Won(finished) => {
                self.copy_page(page);
                if self.copied.get(page) {
                    record_cow_capture(via);
                }
                self.claims.lock().remove(&page);
                let _ = finished.send(true);
                true
            }
        }
    }

    fn claim(&self, page: u32) -> Claim {
        let mut claims = self.claims.lock();
        if self.copied.get(page) || self.canceled.load(Ordering::Acquire) {
            return Claim::Settled;
        }
        if let Some(finished) = claims.get(&page) {
            return Claim::Waiting(finished.clone());
        }
        let (finished, waiting) = watch::channel(false);
        claims.insert(page, waiting);
        Claim::Won(finished)
    }

    fn copy_page(&self, page: u32) {
        let offset = u64::from(page) * self.page_size;
        let copied = self
            .source
            .read_page(offset, self.page_size)
            .map_err(|e| ("source_read_error", format!("CoW window: read page {page}: {e}")))
            .and_then(|bytes| {
                self.sink
                    .write_page(&bytes, offset)
                    .map_err(|e| ("sink_write_error", format!("CoW window: write page {page}: {e}")))
            });
        match copied {
            Ok(()) => self.finish_copy(page),
            Err((reason, error)) => {
                record_cow_cancel(reason);
                self.cancel(error);
            }
        }
    }

    fn finish_copy(&self, page: u32) {
        if let Some(tracker) = &self.tracker {
            tracker.mark_exported(page);
        }
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.settle(Ok(()));
        }
        self.copied.set(page);
    }

    pub async fn sweep(&self, interrupted: &CancellationToken) -> Result<(), String> {
        for page in &self.pages {
            if self.canceled.load(Ordering::Acquire) {
                break;
            }
            if interrupted.is_cancelled() {
                record_cow_cancel("sweep_interrupted");
                self.cancel("CoW window: sweep interrupted: context canceled".to_owned());
                break;
            }
            self.capture(page, CaptureSource::Sweep).await;
        }
        self.wait().await
    }

    pub fn cancel(&self, error: String) {
        let wrapped = format!("{CANCELED}: {error}");
        let _ = self.cancel_cause.set(wrapped.clone());
        self.canceled.store(true, Ordering::Release);
        self.settle(Err(wrapped));
    }

    pub fn cancel_cause(&self) -> Option<String> {
        self.cancel_cause.get().cloned()
    }

    pub async fn wait(&self) -> Result<(), String> {
        let completion = self
            .done
            .subscribe()
            .wait_for(Option::is_some)
            .await
            .map(|completion| completion.clone().unwrap_or(Ok(())))
            .unwrap_or(Ok(()));
        match self.cancel_cause() {
            Some(cause) => Err(cause),
            None => completion,
        }
    }

    pub async fn cancel_and_drain(&self, error: String) {
        self.cancel(error);
        loop {
            let claim = self.claims.lock().values().next().cloned();
            match claim {
                None => return,
                Some(mut finished) => {
                    let _ = finished.wait_for(|finished| *finished).await;
                }
            }
        }
    }

    pub fn copied_pages(&self) -> i64 {
        self.pages.len() as i64 - self.remaining.load(Ordering::Acquire)
    }

    pub fn overlaps(&self, start: u32, end: u32) -> bool {
        !self.canceled.load(Ordering::Acquire) && end > start && self.pages.range_cardinality(start..end) > 0
    }

    pub fn page_set(&self) -> &RoaringBitmap {
        &self.pages
    }

    fn settle(&self, completion: Result<(), String>) {
        self.done.send_if_modified(|current| match current {
            Some(_) => false,
            None => {
                *current = Some(completion);
                true
            }
        });
    }
}

enum Claim {
    Settled,
    Waiting(watch::Receiver<bool>),
    Won(watch::Sender<bool>),
}

#[cfg(test)]
mod tests {
    use futures::future::join_all;

    use super::testing::{LiveMemory, MemorySink, MemorySource, PAGE_SIZE, pseudo_random};
    use super::*;
    use crate::memory::page_tracker::PageState;
    use crate::memory::region::Regions;

    fn bitmap(pages: impl IntoIterator<Item = u32>) -> RoaringBitmap {
        pages.into_iter().collect()
    }

    struct Fixture {
        window: Arc<CowWindow>,
        source: MemorySource,
        sink: Arc<MemorySink>,
        original: Vec<u8>,
    }

    fn fixture(page_count: u32, dirty: impl IntoIterator<Item = u32>, tracker: Option<Arc<PageTracker>>) -> Fixture {
        let size = u64::from(page_count) * PAGE_SIZE;
        let original = pseudo_random(size as usize, 42);
        let source = MemorySource::new(original.clone());
        let sink = MemorySink::new(size);
        let window = Arc::new(CowWindow::new(
            bitmap(dirty),
            PAGE_SIZE,
            Arc::new(source.clone()),
            sink.clone(),
            tracker,
        ));
        Fixture {
            window,
            source,
            sink,
            original,
        }
    }

    fn never_interrupted() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_copies_all() {
        let dirty = [0, 3, 4, 7, 31];
        let fixture = fixture(32, dirty, None);

        fixture.window.sweep(&never_interrupted()).await.unwrap();
        fixture.window.wait().await.unwrap();

        let sunk = fixture.sink.data();
        for page in dirty {
            let offset = u64::from(page) * PAGE_SIZE;
            let range = offset as usize..(offset + PAGE_SIZE) as usize;
            assert_eq!(sunk[range.clone()], fixture.original[range], "page {page} content");
            assert_eq!(fixture.sink.write_count(offset), 1, "page {page} written exactly once");
        }
        assert_eq!(fixture.sink.write_count(PAGE_SIZE), 0, "page 1 is outside the window");
        assert_eq!(fixture.window.copied_pages(), dirty.len() as i64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writers_racing_the_sweep_never_change_the_pre_image() {
        const PAGES: u32 = 256;
        let fixture = fixture(PAGES, 0..PAGES, None);

        let sweeping = tokio::spawn({
            let window = fixture.window.clone();
            async move { window.sweep(&never_interrupted()).await }
        });
        let writers = (0..PAGES).map(|page| {
            let window = fixture.window.clone();
            let source = fixture.source.clone();
            tokio::spawn(async move {
                window.ensure_copied(page).await;
                source.overwrite_page(page, 0xEE);
            })
        });
        join_all(writers).await.into_iter().for_each(|joined| joined.unwrap());
        sweeping.await.unwrap().unwrap();
        fixture.window.wait().await.unwrap();

        assert_eq!(
            fixture.sink.data(),
            fixture.original,
            "sink must hold pre-write content for every page"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writers_racing_the_sweep_through_the_borrowing_fast_path_keep_the_pre_image() {
        const PAGES: u32 = 256;
        let size = u64::from(PAGES) * PAGE_SIZE;
        let original = pseudo_random(size as usize, 43);
        let live = Arc::new(LiveMemory::new(original.clone()));
        let sink = MemorySink::new(size);
        let window = Arc::new(CowWindow::new(
            bitmap(0..PAGES),
            PAGE_SIZE,
            live.clone(),
            sink.clone(),
            None,
        ));

        let sweeping = tokio::spawn({
            let window = window.clone();
            async move { window.sweep(&never_interrupted()).await }
        });
        let writers = (0..PAGES).map(|page| {
            let window = window.clone();
            let live = live.clone();
            tokio::spawn(async move {
                window.ensure_copied(page).await;
                live.overwrite_page(page, 0xEE);
            })
        });
        join_all(writers).await.into_iter().for_each(|joined| joined.unwrap());
        sweeping.await.unwrap().unwrap();
        window.wait().await.unwrap();

        assert_eq!(
            sink.data(),
            original,
            "fast path must capture pre-write content for every page"
        );
        assert_eq!(window.copied_pages(), i64::from(PAGES));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_page_reaches_the_sink_exactly_once() {
        const PAGES: u32 = 64;
        let fixture = fixture(PAGES, 0..PAGES, None);

        let sweeping = tokio::spawn({
            let window = fixture.window.clone();
            async move { window.sweep(&never_interrupted()).await }
        });
        let hammering = (0..8).map(|_| {
            let window = fixture.window.clone();
            tokio::spawn(async move {
                for page in 0..PAGES {
                    window.ensure_copied(page).await;
                }
            })
        });
        join_all(hammering).await.into_iter().for_each(|joined| joined.unwrap());
        sweeping.await.unwrap().unwrap();
        fixture.window.wait().await.unwrap();

        for page in 0..PAGES {
            assert_eq!(
                fixture.sink.write_count(u64::from(page) * PAGE_SIZE),
                1,
                "page {page} written exactly once"
            );
        }
    }

    #[tokio::test]
    async fn a_canceled_window_degrades_to_a_noop() {
        let fixture = fixture(16, 0..16, None);

        fixture.window.cancel("checkpoint aborted".to_owned());

        assert!(
            !fixture.window.ensure_copied(3).await,
            "a canceled window owns no pages"
        );
        assert_eq!(fixture.sink.write_count(3 * PAGE_SIZE), 0, "no copy after cancel");
        assert!(fixture.window.wait().await.unwrap_err().starts_with(CANCELED));
        let swept = fixture.window.sweep(&never_interrupted()).await;
        assert!(
            swept.unwrap_err().starts_with(CANCELED),
            "a sweep on a canceled window resolves to the cancel error"
        );
    }

    #[tokio::test]
    async fn a_sink_error_cancels_the_window() {
        let fixture = fixture(8, 0..8, None);
        fixture.sink.fail_at(4 * PAGE_SIZE);

        let swept = fixture.window.sweep(&never_interrupted()).await;

        assert!(swept.unwrap_err().starts_with(CANCELED));
        assert!(fixture.window.wait().await.unwrap_err().starts_with(CANCELED));
        fixture.window.ensure_copied(7).await;
    }

    #[tokio::test]
    async fn captured_pages_are_rebaselined_and_a_racing_write_stays_dirty() {
        const PAGES: u32 = 32;
        let tracker = Arc::new(PageTracker::default());
        tracker.set_range(0..PAGES, PageState::Dirty);
        let fixture = fixture(PAGES, 0..PAGES, Some(tracker.clone()));

        assert!(fixture.window.ensure_copied(5).await, "page 5 belongs to the window");
        tracker.mark_written(5);
        fixture.window.sweep(&never_interrupted()).await.unwrap();
        fixture.window.wait().await.unwrap();

        for page in 0..PAGES {
            let want = if page == 5 { PageState::Dirty } else { PageState::Clean };
            assert_eq!(tracker.state(page), want, "page {page} state after the sweep");
        }
        assert_eq!(
            fixture.sink.data(),
            fixture.original,
            "rebaseline never changes what is exported"
        );
    }

    #[tokio::test]
    async fn an_empty_page_set_resolves_immediately() {
        let fixture = fixture(4, [], None);

        fixture.window.wait().await.unwrap();
        fixture.window.sweep(&never_interrupted()).await.unwrap();

        assert_eq!(fixture.sink.written_offsets(), 0, "nothing to capture, nothing written");
    }

    #[tokio::test]
    async fn overlaps_trips_on_any_window_page_until_the_window_is_canceled() {
        let dirty = [2, 5, 9];
        let fixture = fixture(16, dirty, None);

        assert!(!fixture.window.overlaps(0, 2), "below the set");
        assert!(!fixture.window.overlaps(3, 5), "gap between set pages");
        assert!(fixture.window.overlaps(0, 16), "window pages in range");
        assert!(fixture.window.overlaps(5, 6), "exact window page");
        assert!(!fixture.window.overlaps(10, 16), "above the set");

        assert!(fixture.window.ensure_copied(5).await);
        assert!(fixture.window.overlaps(5, 6), "a captured page still trips");
        fixture.window.sweep(&never_interrupted()).await.unwrap();
        assert!(fixture.window.overlaps(0, 16), "a fully captured window still trips");
        assert_eq!(
            fixture.sink.written_offsets(),
            dirty.len(),
            "every window page captured once"
        );

        let canceled = self::fixture(16, dirty, None);
        canceled.window.cancel("boom".to_owned());
        assert!(!canceled.window.overlaps(0, 16));
        assert_eq!(canceled.sink.written_offsets(), 0, "a canceled window captured nothing");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn many_small_rounds_keep_the_pre_image() {
        for round in 0..20u32 {
            let pages = 16 + round;
            let fixture = fixture(pages, 0..pages, None);

            let sweeping = tokio::spawn({
                let window = fixture.window.clone();
                async move { window.sweep(&never_interrupted()).await }
            });
            let writers = (0..pages).map(|page| {
                let window = fixture.window.clone();
                let source = fixture.source.clone();
                tokio::spawn(async move {
                    window.ensure_copied(page).await;
                    source.overwrite_first_byte(page, 0xAA);
                })
            });
            join_all(writers).await.into_iter().for_each(|joined| joined.unwrap());
            sweeping.await.unwrap().unwrap();
            fixture.window.wait().await.unwrap();

            assert_eq!(fixture.sink.data(), fixture.original, "round {round} pre-image");
        }
    }

    #[tokio::test]
    async fn a_cancel_that_lost_the_done_race_stays_authoritative() {
        let fixture = fixture(1, [0], None);

        assert!(fixture.window.ensure_copied(0).await);
        fixture
            .window
            .cancel("REMOVE zapped uncaptured window pages".to_owned());

        assert!(
            fixture
                .window
                .sweep(&never_interrupted())
                .await
                .unwrap_err()
                .starts_with(CANCELED)
        );
        assert!(fixture.window.wait().await.unwrap_err().starts_with(CANCELED));
    }

    #[tokio::test]
    async fn an_interrupted_sweep_cancels_the_window() {
        let fixture = fixture(8, 0..8, None);
        let interrupted = CancellationToken::new();
        interrupted.cancel();

        let swept = fixture.window.sweep(&interrupted).await;

        assert!(swept.unwrap_err().contains("sweep interrupted"));
    }

    #[tokio::test]
    async fn cancel_and_drain_returns_when_no_copy_is_in_flight() {
        let fixture = fixture(4, 0..4, None);

        fixture.window.cancel_and_drain("teardown".to_owned()).await;

        assert!(fixture.window.cancel_cause().is_some());
    }

    #[test]
    fn the_packed_sink_translates_identity_offsets_to_packed_slots() {
        let pages = bitmap([2, 5, 9]);
        let destination = MemorySink::new(3 * PAGE_SIZE);
        let sink = PackedPageSink::new(pages, PAGE_SIZE, destination.clone());
        let page = |value: u8| vec![value; PAGE_SIZE as usize];

        sink.write_page(&page(0x99), 9 * PAGE_SIZE).unwrap();
        sink.write_page(&page(0x22), 2 * PAGE_SIZE).unwrap();
        sink.write_page(&page(0x55), 5 * PAGE_SIZE).unwrap();

        let data = destination.data();
        assert_eq!(data[0], 0x22, "page 2 to packed slot 0");
        assert_eq!(data[PAGE_SIZE as usize], 0x55, "page 5 to packed slot 1");
        assert_eq!(data[2 * PAGE_SIZE as usize], 0x99, "page 9 to packed slot 2");
        assert!(
            sink.write_page(&page(0), 3 * PAGE_SIZE).is_err(),
            "a page outside the set must be rejected"
        );
        assert!(
            sink.write_page(&page(0)[..10], 2 * PAGE_SIZE).is_err(),
            "a partial page must be rejected"
        );
        assert!(
            sink.write_page(&page(0), 2 * PAGE_SIZE + 1).is_err(),
            "an unaligned offset must be rejected"
        );
    }

    #[tokio::test]
    async fn a_window_over_the_packed_sink_produces_the_concatenated_pre_images() {
        let dirty = [1u32, 4, 5, 11, 15];
        let sources: [(&str, Arc<dyn PageSource>); 2] = [
            ("readat fallback", Arc::new(MemorySource::new(page_filled_memory(16)))),
            ("slice fast path", Arc::new(LiveMemory::new(page_filled_memory(16)))),
        ];
        for (name, source) in sources {
            let packed = MemorySink::new(dirty.len() as u64 * PAGE_SIZE);
            let pages = bitmap(dirty);
            let window = CowWindow::new(
                pages.clone(),
                PAGE_SIZE,
                source,
                Arc::new(PackedPageSink::new(pages, PAGE_SIZE, packed.clone())),
                None,
            );

            window.sweep(&never_interrupted()).await.unwrap();
            window.wait().await.unwrap();

            let data = packed.data();
            for (slot, page) in dirty.iter().enumerate() {
                assert_eq!(
                    data[slot * PAGE_SIZE as usize],
                    *page as u8,
                    "{name}: packed slot {slot} holds page {page}"
                );
            }
        }
    }

    fn page_filled_memory(pages: usize) -> Vec<u8> {
        (0..pages)
            .flat_map(|page| vec![page as u8; PAGE_SIZE as usize])
            .collect()
    }

    fn two_regions() -> Regions {
        Regions::parse(
            br#"[{"base_host_virt_addr":65536,"size":16384,"offset":0,"page_size":4096},
                 {"base_host_virt_addr":589824,"size":16384,"offset":16384,"page_size":4096}]"#,
        )
        .unwrap()
    }

    #[test]
    fn contiguous_pages_within_one_region_coalesce() {
        let runs = coalesce_write_protect_runs(&bitmap([0, 1, 2]), &two_regions()).unwrap();

        assert_eq!(
            runs,
            vec![WriteProtectRun {
                start: 0x10000,
                length: 3 * PAGE_SIZE
            }]
        );
    }

    #[test]
    fn a_gap_in_the_page_set_splits_the_run() {
        let runs = coalesce_write_protect_runs(&bitmap([0, 2, 3]), &two_regions()).unwrap();

        assert_eq!(
            runs,
            vec![
                WriteProtectRun {
                    start: 0x10000,
                    length: PAGE_SIZE
                },
                WriteProtectRun {
                    start: 0x10000 + 2 * PAGE_SIZE,
                    length: 2 * PAGE_SIZE
                },
            ]
        );
    }

    #[test]
    fn a_region_boundary_splits_index_adjacent_pages() {
        let runs = coalesce_write_protect_runs(&bitmap([2, 3, 4, 5]), &two_regions()).unwrap();

        assert_eq!(
            runs,
            vec![
                WriteProtectRun {
                    start: 0x10000 + 2 * PAGE_SIZE,
                    length: 2 * PAGE_SIZE
                },
                WriteProtectRun {
                    start: 0x90000,
                    length: 2 * PAGE_SIZE
                },
            ]
        );
    }

    #[test]
    fn an_out_of_range_page_fails_the_translation() {
        assert!(coalesce_write_protect_runs(&bitmap([9]), &two_regions()).is_err());
    }

    #[test]
    fn an_empty_set_arms_nothing() {
        assert!(
            coalesce_write_protect_runs(&RoaringBitmap::new(), &two_regions())
                .unwrap()
                .is_empty()
        );
    }
}
