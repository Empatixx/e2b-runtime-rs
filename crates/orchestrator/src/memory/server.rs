use std::fs::File;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use roaring::RoaringBitmap;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use self::deferred::DeferredFaults;
use self::read_retry::with_read_retries;
use self::retry_timer::RetryTimer;
use self::state::SharedState;
use super::balloon_mode::BalloonMode;
use super::cow_window::{CowWindow, PageSink, PageSource, coalesce_write_protect_runs};
use super::page_tracker::{ExportedPages, PageState};
use super::prefetch::{Installer, Prefaulted};
use super::region::Regions;
use super::trace::{Access, Trace};
use super::uffd::{CopyOutcome, Event, Uffd};
use crate::block::build_file::BuildFile;
use crate::block::page_data::PageData;
use crate::telemetry::uffd::{FaultResult, PageClass, record_memfile_leg_fault, record_prefault, record_serve};

mod counters;
mod deferred;
mod read_retry;
mod resolve;
mod retry_timer;
mod state;
mod wp_failures;
mod write_protect;

pub use self::counters::ServeStats;
pub use self::write_protect::Support;

const MAX_FAULTS_IN_FLIGHT: usize = 4096;
const RETRY_DELAY: Duration = Duration::from_millis(1);
const STOPPED: &str = "the memory server has stopped";
const SMALL_PAGE: u64 = 4096;

type Settled = oneshot::Sender<()>;

pub struct MemoryServer {
    stop: oneshot::Sender<()>,
    settles: mpsc::UnboundedSender<Settled>,
    running: watch::Receiver<()>,
    task: JoinHandle<Result<(), String>>,
    state: Arc<SharedState>,
    uffd: Arc<AsyncFd<Uffd>>,
    regions: Regions,
    memfd: Option<Arc<File>>,
}

pub struct MemoryWatch {
    running: watch::Receiver<()>,
    state: Arc<SharedState>,
}

#[derive(Clone)]
pub struct Prefaulter {
    uffd: Arc<AsyncFd<Uffd>>,
    regions: Regions,
    state: Arc<SharedState>,
}

async fn request_settle(settles: &mpsc::UnboundedSender<Settled>) -> Result<(), String> {
    let (settled, reply) = oneshot::channel();
    settles.send(settled).map_err(|_| STOPPED.to_owned())?;
    reply.await.map_err(|_| STOPPED.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fault {
    address: u64,
    write: bool,
    write_protect: bool,
}

struct Serving {
    uffd: Arc<AsyncFd<Uffd>>,
    regions: Regions,
    memfile: Arc<BuildFile>,
    state: Arc<SharedState>,
}

impl MemoryServer {
    pub fn start(uffd: Uffd, regions: Regions, memfile: Arc<BuildFile>, memfd: Option<File>) -> io::Result<Self> {
        let generation = memfile.header().metadata.generation;
        let state = Arc::new(SharedState::new(regions.page_size(), generation));
        let uffd = Arc::new(AsyncFd::new(uffd)?);
        let serving = Serving {
            uffd: uffd.clone(),
            regions: regions.clone(),
            memfile,
            state: state.clone(),
        };
        let (stop, stopped) = oneshot::channel();
        let (settles, settling) = mpsc::unbounded_channel();
        let (alive, running) = watch::channel(());
        let ending = state.clone();
        let task = tokio::spawn(async move {
            let served = serving.run(stopped, settling).await;
            ending.close();
            if let Err(error) = &served {
                ending.fail(error.clone());
            }
            drop(alive);
            served
        });
        Ok(Self {
            stop,
            settles,
            running,
            task,
            state,
            uffd,
            regions,
            memfd: memfd.map(Arc::new),
        })
    }

    pub fn memfd(&self) -> Option<Arc<File>> {
        self.memfd.clone()
    }

    pub fn page_size(&self) -> u64 {
        self.regions.page_size()
    }

    pub fn trace(&self) -> Trace {
        self.state.trace()
    }

    pub fn prefaulter(&self) -> Arc<Prefaulter> {
        Arc::new(Prefaulter {
            uffd: self.uffd.clone(),
            regions: self.regions.clone(),
            state: self.state.clone(),
        })
    }

    pub fn prefault(&self, offset: u64, data: &[u8]) -> Result<Prefaulted, String> {
        self.prefaulter().prefault(offset, data)
    }

    pub fn faults_served(&self) -> u64 {
        self.state.stats().pages.max(0) as u64
    }

    pub fn serve_stats(&self) -> ServeStats {
        self.state.stats()
    }

    pub fn serve_reader(&self) -> impl Fn() -> ServeStats + Send + Sync + 'static {
        let state = self.state.clone();
        move || state.stats()
    }

    pub fn wp_faults_resolved(&self) -> i64 {
        self.state.stats().wp_faults
    }

    pub fn set_balloon_mode(&self, mode: BalloonMode) {
        self.state.set_balloon_mode(mode);
    }

    pub fn write_protecting(&self) -> bool {
        self.state.write_protecting()
    }

    pub fn write_protect_support(&self) -> Support {
        self.state.write_protect_support()
    }

    pub fn empty_pages(&self) -> RoaringBitmap {
        self.state.tracker.export().empty
    }

    pub fn page_state(&self, page: u32) -> PageState {
        self.state.page_state(page)
    }

    pub async fn export_page_states(&self) -> Result<ExportedPages, String> {
        self.settle().await?;
        Ok(self.state.tracker.export())
    }

    pub fn exporter(&self) -> CowExporter {
        CowExporter {
            uffd: self.uffd.clone(),
            regions: self.regions.clone(),
            state: self.state.clone(),
            settles: self.settles.clone(),
        }
    }

    pub fn cancel_active_cow_window(&self, error: String) {
        if let Some(window) = self.state.take_cow_window() {
            crate::telemetry::uffd::record_cow_cancel("teardown");
            window.cancel_and_drain(error);
        }
    }

    pub async fn settle(&self) -> Result<(), String> {
        request_settle(&self.settles).await
    }

    pub fn watch(&self) -> MemoryWatch {
        MemoryWatch {
            running: self.running.clone(),
            state: self.state.clone(),
        }
    }

    pub async fn ended(&self) {
        self.watch().ended().await
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn stop(self) -> Result<(), String> {
        let _ = self.stop.send(());
        let served = self.task.await.map_err(|e| format!("memory server task: {e}"))?;
        self.state.take_cow_window().into_iter().for_each(|window| {
            crate::telemetry::uffd::record_cow_cancel("teardown");
            window.cancel_and_drain("memory server stopped".to_owned());
        });
        served
    }
}

#[derive(Clone)]
pub struct CowExporter {
    uffd: Arc<AsyncFd<Uffd>>,
    regions: Regions,
    state: Arc<SharedState>,
    settles: mpsc::UnboundedSender<Settled>,
}

impl CowExporter {
    pub fn arm_write_protect(&self, pages: &RoaringBitmap) -> Result<(), String> {
        coalesce_write_protect_runs(pages, &self.regions)
            .map_err(|e| format!("arm write-protect: {e}"))?
            .into_iter()
            .try_for_each(|run| {
                self.uffd
                    .get_ref()
                    .write_protect(run.start, run.length, true)
                    .map_err(|e| {
                        format!(
                            "arm write-protect [{:#x}, {:#x}): {e}",
                            run.start,
                            run.start + run.length
                        )
                    })
            })
    }

    pub fn begin(
        &self,
        pages: &RoaringBitmap,
        source: Arc<dyn PageSource>,
        sink: Arc<dyn PageSink>,
    ) -> Result<Arc<CowWindow>, String> {
        self.arm_write_protect(pages)?;
        let window = Arc::new(CowWindow::new(
            pages.clone(),
            self.regions.page_size(),
            source,
            sink,
            Some(self.state.tracker.clone()),
        ));
        self.state.install_cow_window(window.clone());
        Ok(window)
    }

    pub async fn end(&self, window: &Arc<CowWindow>) {
        let _ = request_settle(&self.settles).await;
        self.state.uninstall_cow_window(window);
    }
}

impl MemoryWatch {
    pub async fn ended(mut self) {
        while self.running.changed().await.is_ok() {}
    }

    pub async fn failed(self) -> String {
        let state = self.state.clone();
        self.ended().await;
        match state.failure() {
            Some(error) => error,
            None => std::future::pending().await,
        }
    }
}

impl Installer for Prefaulter {
    fn page_size(&self) -> u64 {
        self.regions.page_size()
    }

    fn prefault(&self, offset: u64, data: &[u8]) -> Result<Prefaulted, String> {
        let started = Instant::now();
        let page_size = self.regions.page_size();
        let installing = self.state.installing();
        if installing.closed {
            return Ok(Prefaulted::Closed);
        }
        let (prefaulted, result) = self.install_prefetched(offset, data, page_size)?;
        let bytes = match prefaulted {
            Prefaulted::Installed => page_size,
            _ => 0,
        };
        record_prefault(self.state.generation, result, started.elapsed(), bytes);
        Ok(prefaulted)
    }
}

impl Prefaulter {
    fn install_prefetched(
        &self,
        offset: u64,
        data: &[u8],
        page_size: u64,
    ) -> Result<(Prefaulted, FaultResult), String> {
        let Some(address) = self.regions.address_of(offset) else {
            return Err(format!(
                "failed to get host virtual address: offset {offset:#x} is outside guest memory"
            ));
        };
        if data.len() as u64 != page_size {
            return Err(format!(
                "data length ({}) does not match pagesize ({page_size})",
                data.len()
            ));
        }
        let page = (offset / page_size) as u32;
        if !self.state.is_untouched(page) {
            return Ok((Prefaulted::Skipped, FaultResult::Skipped));
        }
        match self.state.copy_page(self.uffd.get_ref(), address, data, true)? {
            CopyOutcome::Copied => {
                self.state.installed(page, Access::Prefetch, false);
                Ok((Prefaulted::Installed, FaultResult::Installed))
            }
            CopyOutcome::AlreadyPresent => {
                self.state.trace_prefetch(page);
                Ok((Prefaulted::Present, FaultResult::Present))
            }
            CopyOutcome::Retry | CopyOutcome::NoWriteProtect => Ok((Prefaulted::Deferred, FaultResult::Deferred)),
            CopyOutcome::Gone => Ok((Prefaulted::Skipped, FaultResult::Discarded)),
        }
    }
}

impl Serving {
    async fn run(
        self,
        mut stopped: oneshot::Receiver<()>,
        mut settles: mpsc::UnboundedReceiver<Settled>,
    ) -> Result<(), String> {
        let mut in_flight = FuturesUnordered::new();
        let mut deferred = DeferredFaults::default();
        let mut waiting = Vec::new();
        let mut retry_timer = RetryTimer::new(RETRY_DELAY);
        loop {
            let room = in_flight.len() < MAX_FAULTS_IN_FLIGHT;
            tokio::select! {
                _ = &mut stopped => return Ok(()),
                Some(waiter) = settles.recv() => waiting.push(waiter),
                events = self.next_events(&mut retry_timer, !deferred.is_empty()), if room => {
                    let faults = match events {
                        Ok(events) => self.record_removals(events),
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                        Err(e) => return Err(format!("read uffd events: {e}")),
                    };
                    let mut dispatch = DeferredFaults::default();
                    dispatch.extend(deferred.drain());
                    dispatch.extend(faults);
                    in_flight.extend(dispatch.drain().into_iter().map(|fault| self.serve_fault(fault)));
                }
                Some(served) = in_flight.next() => {
                    let was_idle = deferred.is_empty();
                    if let Some(retry) = served? {
                        deferred.push(retry);
                    }
                    if was_idle && !deferred.is_empty() {
                        retry_timer.arm();
                    }
                }
            }
            if in_flight.is_empty() && deferred.is_empty() {
                let faults = self.settle_waiting(&mut waiting)?;
                in_flight.extend(faults.into_iter().map(|fault| self.serve_fault(fault)));
            }
        }
    }

    fn settle_waiting(&self, waiting: &mut Vec<Settled>) -> Result<Vec<Fault>, String> {
        if waiting.is_empty() {
            return Ok(Vec::new());
        }
        let faults = self.record_removals(self.queued_events()?);
        if faults.is_empty() {
            waiting.drain(..).for_each(|waiter| {
                let _ = waiter.send(());
            });
        }
        Ok(faults)
    }

    fn queued_events(&self) -> Result<Vec<Event>, String> {
        match self.uffd.get_ref().read_events() {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(Vec::new()),
            events => events.map_err(|e| format!("read uffd events: {e}")),
        }
    }

    async fn next_events(&self, retry_timer: &mut RetryTimer, retrying: bool) -> io::Result<Vec<Event>> {
        if retrying {
            retry_timer.elapsed().await;
            return self.uffd.get_ref().read_events();
        }
        let mut ready = self.uffd.readable().await?;
        let events = ready.get_inner().read_events()?;
        if events.is_empty() {
            ready.clear_ready();
        }
        Ok(events)
    }

    fn record_removals(&self, events: Vec<Event>) -> Vec<Fault> {
        events
            .into_iter()
            .filter_map(|event| match event {
                Event::PageFault {
                    address,
                    write,
                    write_protect,
                } => Some(Fault {
                    address,
                    write,
                    write_protect,
                }),
                Event::Remove { start, end } => {
                    self.mark_removed(start, end);
                    None
                }
            })
            .collect()
    }

    fn mark_removed(&self, start: u64, end: u64) {
        let page_size = self.regions.page_size();
        match self.regions.offset_of(start) {
            Ok(offset) => {
                let first = (offset / page_size) as u32;
                let pages = ((end - start) / page_size) as u32;
                self.state.mark_removed(first..first + pages);
            }
            Err(e) => tracing::error!(start, error = %e, "UFFD REMOVE: failed to map start address"),
        }
    }

    async fn serve_fault(&self, fault: Fault) -> Result<Option<Fault>, String> {
        match fault.write_protect {
            true => self.resolve_write_protect(fault),
            false => self.serve_missing(fault).await,
        }
    }

    async fn serve_missing(&self, fault: Fault) -> Result<Option<Fault>, String> {
        let started = Instant::now();
        let page = fault.address & !(self.regions.page_size() - 1);
        let mut served = Served::default();
        let outcome = self
            .install(page, fault.write, &mut served)
            .await
            .map_err(|e| format!("serve page at {page:#x}: {e}"));
        if outcome.is_err() {
            served.result = FaultResult::Error;
        }
        self.state
            .counters
            .record_serve(served.class, served.result, served.bytes);
        record_serve(
            self.state.balloon_mode(),
            self.state.generation,
            served.class,
            served.result,
            started.elapsed(),
            served.bytes.max(0) as u64,
        );
        outcome.map(|outcome| matches!(outcome, CopyOutcome::Retry).then_some(fault))
    }

    async fn install(&self, page: u64, write: bool, served: &mut Served) -> Result<CopyOutcome, String> {
        let offset = self.regions.offset_of(page)?;
        let index = (offset / self.regions.page_size()) as u32;
        let source = match self.state.page_state(index) {
            PageState::Dirty | PageState::Clean => {
                served.class = PageClass::Resident;
                served.result = FaultResult::Present;
                return Ok(CopyOutcome::AlreadyPresent);
            }
            PageState::Zero | PageState::Removed => {
                served.class = PageClass::Zero;
                None
            }
            PageState::NotPresent => {
                served.class = PageClass::New;
                Some(
                    with_read_retries(|| self.page_bytes(offset))
                        .await
                        .inspect_err(|_| record_memfile_leg_fault())?,
                )
            }
        };
        self.install_bytes(page, index, source, write, served)
    }

    fn install_bytes(
        &self,
        page: u64,
        index: u32,
        source: Option<PageData>,
        write: bool,
        served: &mut Served,
    ) -> Result<CopyOutcome, String> {
        let page_size = self.regions.page_size();
        let _installing = self.state.installing();
        let bytes = match (self.state.page_state(index), source) {
            (PageState::Dirty | PageState::Clean, _) => {
                served.class = PageClass::Resident;
                served.result = FaultResult::Present;
                return Ok(CopyOutcome::AlreadyPresent);
            }
            (PageState::Zero | PageState::Removed, _) | (PageState::NotPresent, None) => None,
            (PageState::NotPresent, Some(bytes)) => Some(bytes),
        };
        let zero_filled = bytes.is_none();
        let outcome = match (&bytes, write, page_size) {
            (None, true, SMALL_PAGE) => self.uffd.get_ref().zero(page, page_size).map_err(|e| e.to_string())?,
            (None, _, _) => {
                self.state
                    .copy_page(self.uffd.get_ref(), page, &PageData::zeroes(page_size as usize), !write)?
            }
            (Some(bytes), _, _) => self.state.copy_page(self.uffd.get_ref(), page, bytes, !write)?,
        };
        let access = if write { Access::Write } else { Access::Read };
        match outcome {
            CopyOutcome::Copied => {
                served.result = FaultResult::Installed;
                served.bytes = page_size as i64;
                self.state.installed(index, access, zero_filled);
            }
            CopyOutcome::AlreadyPresent => {
                served.result = FaultResult::Present;
                self.uffd.get_ref().wake(page, page_size).map_err(|e| e.to_string())?;
                self.state.installed(index, access, zero_filled);
            }
            CopyOutcome::Retry => served.result = FaultResult::Deferred,
            CopyOutcome::Gone => served.result = FaultResult::Discarded,
            CopyOutcome::NoWriteProtect => {}
        }
        Ok(outcome)
    }

    async fn page_bytes(&self, offset: u64) -> Result<PageData, String> {
        self.memfile.slice(offset, self.regions.page_size() as usize).await
    }
}

struct Served {
    class: PageClass,
    result: FaultResult,
    bytes: i64,
}

impl Default for Served {
    fn default() -> Self {
        Self {
            class: PageClass::Unknown,
            result: FaultResult::Installed,
            bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use uuid::Uuid;

    use super::*;
    use crate::block::build_file::{Artifact, DiffStore};
    use crate::memory::uffd::testing::{Pages, available};
    use crate::storage::backend::Storage;
    use crate::storage::header::mapping::BuildMapping;
    use crate::storage::header::{Header, Metadata};

    const PAGE: u64 = 4096;

    struct Fixture {
        _dir: tempfile::TempDir,
        pages: Arc<Pages>,
        server: MemoryServer,
    }

    async fn fixture(mapped_pages: u64, memfile_pages: u64) -> Fixture {
        fixture_with(mapped_pages, memfile_pages, false).await
    }

    async fn fixture_with(mapped_pages: u64, memfile_pages: u64, write_protected: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let build_id = Uuid::new_v4();
        let content: Vec<u8> = (0..memfile_pages)
            .flat_map(|page| vec![page as u8 + 1; PAGE as usize])
            .collect();
        storage
            .put(&Artifact::Memfile.object(build_id), Bytes::from(content))
            .await
            .unwrap();
        let header = Header::new(Metadata::template(build_id, PAGE, memfile_pages * PAGE), Vec::new()).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let memfile = Arc::new(BuildFile::new(header, Artifact::Memfile, store));

        let pages = Arc::new(Pages::map((mapped_pages * PAGE) as usize));
        let uffd = match write_protected {
            true => pages.register_write_protected().unwrap(),
            false => pages.register().unwrap(),
        };
        let regions = Regions::parse(
            format!(
                r#"[{{"base_host_virt_addr":{},"size":{},"offset":0,"page_size_kib":4096}}]"#,
                pages.address(),
                mapped_pages * PAGE
            )
            .as_bytes(),
        )
        .unwrap();
        let server = MemoryServer::start(uffd, regions, memfile, None).unwrap();
        Fixture {
            _dir: dir,
            pages,
            server,
        }
    }

    async fn eventually(condition: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !condition() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        condition()
    }

    async fn read_page(pages: &Arc<Pages>, page: u64) -> u8 {
        let pages = pages.clone();
        tokio::task::spawn_blocking(move || pages.read_byte((page * PAGE + 11) as usize))
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serves_faults_with_memfile_bytes() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(3, 3).await;

        let (first, second, third) = tokio::join!(
            read_page(&fixture.pages, 0),
            read_page(&fixture.pages, 1),
            read_page(&fixture.pages, 2)
        );

        assert_eq!((first, second, third), (1, 2, 3));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while fixture.server.faults_served() < 3 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(fixture.server.faults_served(), 3);
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn removed_pages_fault_back_as_zero() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(2, 2).await;
        assert_eq!(read_page(&fixture.pages, 1).await, 2);

        let pages = fixture.pages.clone();
        tokio::task::spawn_blocking(move || pages.discard(PAGE as usize, PAGE as usize))
            .await
            .unwrap();

        assert_eq!(read_page(&fixture.pages, 1).await, 0);
        assert_eq!(read_page(&fixture.pages, 0).await, 1);
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_page_the_memfile_cannot_serve_stops_the_server_with_an_error() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(4, 2).await;
        let address = fixture.pages.address() + 3 * PAGE;
        let pages = fixture.pages.clone();
        let reader = std::thread::spawn(move || pages.read_byte(3 * PAGE as usize));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !fixture.server.is_finished() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let error = fixture.server.stop().await.unwrap_err();
        let _ = reader.join();

        assert!(error.starts_with(&format!("serve page at {address:#x}: ")), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn read_faults_are_write_protected() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        if Pages::map(PAGE as usize).register_write_protected().is_err() {
            eprintln!("skipping: this kernel has no async uffd write-protect for anonymous memory");
            return;
        }
        let fixture = fixture_with(2, 2, true).await;

        assert_eq!(read_page(&fixture.pages, 0).await, 1);

        assert!(eventually(|| fixture.server.write_protecting()).await);
        assert!(Pages::write_protected(fixture.pages.address()));
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn falls_back_to_plain_copies_without_write_protect() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(2, 2).await;

        assert_eq!(read_page(&fixture.pages, 0).await, 1);

        assert!(!fixture.server.write_protecting());
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn removed_pages_are_reported() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(2, 2).await;
        assert_eq!(read_page(&fixture.pages, 1).await, 2);
        let pages = fixture.pages.clone();
        tokio::task::spawn_blocking(move || pages.discard(PAGE as usize, PAGE as usize))
            .await
            .unwrap();
        assert_eq!(read_page(&fixture.pages, 1).await, 0);

        assert_eq!(fixture.server.empty_pages().iter().collect::<Vec<_>>(), vec![1]);
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ended_resolves_when_the_server_stops_on_an_error() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(4, 2).await;
        let pages = fixture.pages.clone();
        let reader = std::thread::spawn(move || pages.read_byte(3 * PAGE as usize));

        let ended = tokio::time::timeout(Duration::from_secs(5), fixture.server.ended()).await;
        let error = fixture.server.stop().await.unwrap_err();
        let _ = reader.join();

        assert!(ended.is_ok(), "ended() did not resolve");
        assert!(error.starts_with("serve page at"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_ends_an_idle_server() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(1, 1).await;

        tokio::time::timeout(Duration::from_secs(2), fixture.server.stop())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_slow_page_does_not_hold_up_other_faults_or_stop() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let (slow, fast) = (Uuid::new_v4(), Uuid::new_v4());
        for build in [slow, fast] {
            storage
                .put(&Artifact::Memfile.object(build), Bytes::from(vec![7; PAGE as usize]))
                .await
                .unwrap();
        }
        let mappings = vec![
            BuildMapping {
                offset: 0,
                length: PAGE,
                build_id: slow,
                storage_offset: 0,
            },
            BuildMapping {
                offset: PAGE,
                length: PAGE,
                build_id: fast,
                storage_offset: 0,
            },
        ];
        let header = Header::new(Metadata::template(fast, PAGE, 2 * PAGE), mappings).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        store.chunker(slow, Artifact::Memfile).await.unwrap();
        let slow_object = dir.path().join("store").join(Artifact::Memfile.object(slow));
        std::fs::remove_file(&slow_object).unwrap();
        nix::unistd::mkfifo(&slow_object, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let memfile = Arc::new(BuildFile::new(header, Artifact::Memfile, store));
        let pages = Arc::new(Pages::map(2 * PAGE as usize));
        let regions = Regions::parse(
            format!(
                r#"[{{"base_host_virt_addr":{},"size":{},"offset":0,"page_size_kib":4096}}]"#,
                pages.address(),
                2 * PAGE
            )
            .as_bytes(),
        )
        .unwrap();
        let server = MemoryServer::start(pages.register().unwrap(), regions, memfile, None).unwrap();

        let stuck = std::thread::spawn({
            let pages = pages.clone();
            move || pages.read_byte(0)
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let fast_page = tokio::time::timeout(Duration::from_secs(2), read_page(&pages, 1)).await;
        let stopped = tokio::time::timeout(Duration::from_secs(2), server.stop()).await;
        drop(std::fs::OpenOptions::new().write(true).open(&slow_object));
        let _ = stuck.join();

        assert_eq!(fast_page.ok(), Some(7), "a fault behind a slow page was not served");
        assert!(stopped.is_ok(), "stop waited for a slow page");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn prefaulted_pages_need_no_fault_and_are_traced() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(3, 3).await;
        let page = vec![9; PAGE as usize];

        assert_eq!(fixture.server.prefault(PAGE, &page), Ok(Prefaulted::Installed));
        assert_eq!(fixture.server.prefault(PAGE, &page), Ok(Prefaulted::Skipped));
        assert_eq!(
            fixture.server.prefault(10 * PAGE, &page),
            Err("failed to get host virtual address: offset 0xa000 is outside guest memory".to_owned())
        );
        assert_eq!(read_page(&fixture.pages, 1).await, 9);
        assert_eq!(read_page(&fixture.pages, 0).await, 1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while fixture.server.faults_served() < 1 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let trace = fixture.server.trace();
        assert_eq!(fixture.server.faults_served(), 1);
        assert_eq!(
            trace
                .touches
                .iter()
                .map(|(index, touch)| (*index, touch.access))
                .collect::<Vec<_>>(),
            vec![(0, Access::Read), (1, Access::Prefetch)]
        );
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn settling_records_the_removals_queued_before_it() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(2, 2).await;
        assert_eq!(read_page(&fixture.pages, 1).await, 2);
        let pages = fixture.pages.clone();
        tokio::task::spawn_blocking(move || pages.discard(PAGE as usize, PAGE as usize))
            .await
            .unwrap();

        fixture.server.settle().await.unwrap();

        assert_eq!(fixture.server.empty_pages().iter().collect::<Vec<_>>(), vec![1]);
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_settle_to_a_dropped_server_reports_it_stopped() {
        let (settles, dropped) = mpsc::unbounded_channel();
        drop(dropped);

        let error = request_settle(&settles).await.unwrap_err();

        assert_eq!(error, STOPPED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn prefaulting_a_stopped_server_reports_it_closed() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(2, 2).await;
        let prefaulter = fixture.server.prefaulter();
        let page = vec![9; PAGE as usize];
        fixture.server.stop().await.unwrap();

        assert_eq!(prefaulter.prefault(0, &page), Ok(Prefaulted::Closed));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_watch_reports_why_the_server_failed() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(4, 2).await;
        let address = fixture.pages.address() + 3 * PAGE;
        let pages = fixture.pages.clone();
        let reader = std::thread::spawn(move || pages.read_byte(3 * PAGE as usize));

        let failure = tokio::time::timeout(Duration::from_secs(5), fixture.server.watch().failed()).await;
        let _ = fixture.server.stop().await;
        let _ = reader.join();

        assert!(failure.unwrap().starts_with(&format!("serve page at {address:#x}: ")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_watch_stays_silent_when_the_server_stops_cleanly() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let fixture = fixture(1, 1).await;
        let watch = fixture.server.watch();
        fixture.server.stop().await.unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(200), watch.failed())
                .await
                .is_err()
        );
    }

    struct SyncFixture {
        pages: Arc<Pages>,
        server: MemoryServer,
        memfile: Arc<BuildFile>,
        _dir: tempfile::TempDir,
    }

    async fn sync_fixture(page_count: u64) -> Option<SyncFixture> {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let build_id = Uuid::new_v4();
        let content: Vec<u8> = (0..page_count)
            .flat_map(|page| vec![page as u8 + 1; PAGE as usize])
            .collect();
        storage
            .put(&Artifact::Memfile.object(build_id), Bytes::from(content))
            .await
            .unwrap();
        let header = Header::new(Metadata::template(build_id, PAGE, page_count * PAGE), Vec::new()).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let memfile = Arc::new(BuildFile::new(header, Artifact::Memfile, store));
        let pages = Arc::new(Pages::map((page_count * PAGE) as usize));
        let Some(uffd) = pages.register_sync_write_protected() else {
            eprintln!("skipping: this kernel has no uffd write-protect for anonymous memory");
            return None;
        };
        let regions = Regions::parse(
            format!(
                r#"[{{"base_host_virt_addr":{},"size":{},"offset":0,"page_size":4096}}]"#,
                pages.address(),
                page_count * PAGE
            )
            .as_bytes(),
        )
        .unwrap();
        let server = MemoryServer::start(uffd, regions, memfile.clone(), None).unwrap();
        Some(SyncFixture {
            pages,
            server,
            memfile,
            _dir: dir,
        })
    }

    async fn write_page(pages: &Arc<Pages>, page: u64, value: u8) {
        let pages = pages.clone();
        tokio::task::spawn_blocking(move || pages.write_byte((page * PAGE) as usize, value))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_write_to_a_protected_page_is_resolved_and_promoted_to_dirty() {
        let Some(fixture) = sync_fixture(2).await else { return };
        assert_eq!(read_page(&fixture.pages, 0).await, 1);
        assert!(eventually(|| fixture.server.page_state(0) == PageState::Clean).await);

        write_page(&fixture.pages, 0, 0x42).await;

        assert!(
            eventually(|| fixture.server.page_state(0) == PageState::Dirty).await,
            "resolve must promote the page to dirty, got {:?}",
            fixture.server.page_state(0)
        );
        assert!(eventually(|| fixture.server.wp_faults_resolved() == 1).await);
        assert_eq!(fixture.pages.read_byte(0), 0x42);
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stale_write_protect_fault_on_an_absent_page_installs_the_armed_zero_page() {
        let Some(fixture) = sync_fixture(2).await else { return };
        assert_eq!(read_page(&fixture.pages, 1).await, 2);
        let pages = fixture.pages.clone();
        tokio::task::spawn_blocking(move || pages.discard(PAGE as usize, PAGE as usize))
            .await
            .unwrap();
        fixture.server.settle().await.unwrap();
        assert_eq!(fixture.server.page_state(1), PageState::Removed);

        let wake = Fault {
            address: fixture.pages.address() + PAGE,
            write: true,
            write_protect: true,
        };
        let serving = Serving {
            uffd: fixture.server.uffd.clone(),
            regions: fixture.server.regions.clone(),
            memfile: fixture.memfile.clone(),
            state: fixture.server.state.clone(),
        };
        assert_eq!(serving.resolve_write_protect(wake), Ok(None));

        assert_eq!(fixture.server.page_state(1), PageState::Zero);
        assert!(
            Pages::write_protected(fixture.pages.address() + PAGE),
            "the probe must install armed"
        );
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_cow_fault_path_captures_the_pre_image_before_the_write_lands() {
        use crate::memory::cow_window::{PageSink, PageSource};
        use std::borrow::Cow;

        struct LiveSource(Arc<Pages>);
        impl PageSource for LiveSource {
            fn read_page(&self, offset: u64, length: u64) -> io::Result<Cow<'_, [u8]>> {
                Ok(Cow::Owned(
                    (0..length as usize)
                        .map(|at| self.0.read_byte(offset as usize + at))
                        .collect(),
                ))
            }
        }
        struct Capture(parking_lot::Mutex<Vec<(u64, Vec<u8>)>>);
        impl PageSink for Capture {
            fn write_page(&self, data: &[u8], offset: u64) -> io::Result<()> {
                self.0.lock().push((offset, data.to_vec()));
                Ok(())
            }
        }

        let Some(fixture) = sync_fixture(4).await else { return };
        for page in 0..4 {
            read_page(&fixture.pages, page).await;
        }
        let sink = Arc::new(Capture(parking_lot::Mutex::default()));
        let dirty: RoaringBitmap = (0..4).collect();
        let window = fixture
            .server
            .exporter()
            .begin(&dirty, Arc::new(LiveSource(fixture.pages.clone())), sink.clone())
            .unwrap();

        write_page(&fixture.pages, 0, 0x99).await;

        let captured = sink.0.lock().clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].0, 0);
        assert!(
            captured[0].1.iter().all(|byte| *byte == 1),
            "the sink must hold the pre-write content"
        );
        window.sweep(&tokio_util::sync::CancellationToken::new()).await.unwrap();
        fixture.server.exporter().end(&window).await;
        fixture.server.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_remove_against_a_window_page_trips_the_window() {
        use crate::memory::cow_window::{PageSink, PageSource};
        use std::borrow::Cow;

        struct Zeros;
        impl PageSource for Zeros {
            fn read_page(&self, _offset: u64, length: u64) -> io::Result<Cow<'_, [u8]>> {
                Ok(Cow::Owned(vec![0; length as usize]))
            }
        }
        struct Void;
        impl PageSink for Void {
            fn write_page(&self, _data: &[u8], _offset: u64) -> io::Result<()> {
                Ok(())
            }
        }

        let Some(fixture) = sync_fixture(4).await else { return };
        for page in 0..4 {
            read_page(&fixture.pages, page).await;
        }
        let dirty: RoaringBitmap = (0..4).collect();
        let window = fixture
            .server
            .exporter()
            .begin(&dirty, Arc::new(Zeros), Arc::new(Void))
            .unwrap();

        let pages = fixture.pages.clone();
        tokio::task::spawn_blocking(move || pages.discard(PAGE as usize, PAGE as usize))
            .await
            .unwrap();
        fixture.server.settle().await.unwrap();

        let cause = window.cancel_cause().unwrap_or_default();
        assert!(cause.contains("REMOVE zapped window pages"), "{cause}");
        let swept = window.sweep(&tokio_util::sync::CancellationToken::new()).await;
        assert!(swept.unwrap_err().contains("memory CoW window canceled"));
        fixture.server.stop().await.unwrap();
    }
}
