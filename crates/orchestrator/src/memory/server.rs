use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use parking_lot::Mutex;
use roaring::RoaringBitmap;
use tokio::io::unix::AsyncFd;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::region::Regions;
use super::uffd::{CopyOutcome, Event, Uffd};
use crate::block::build_file::BuildFile;

const MAX_FAULTS_IN_FLIGHT: usize = 4096;
const RETRY_DELAY: Duration = Duration::from_millis(1);

pub struct MemoryServer {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), String>>,
    state: Arc<SharedState>,
}

#[derive(Default)]
struct SharedState {
    served: AtomicU64,
    write_protect: AtomicBool,
    removed: Mutex<RoaringBitmap>,
}

#[derive(Debug, Clone, Copy)]
struct Fault {
    address: u64,
    write: bool,
}

struct Serving {
    uffd: AsyncFd<Uffd>,
    regions: Regions,
    memfile: Arc<BuildFile>,
    state: Arc<SharedState>,
}

impl MemoryServer {
    pub fn start(uffd: Uffd, regions: Regions, memfile: Arc<BuildFile>) -> io::Result<Self> {
        let state = Arc::new(SharedState {
            write_protect: AtomicBool::new(true),
            ..SharedState::default()
        });
        let serving = Serving {
            uffd: AsyncFd::new(uffd)?,
            regions,
            memfile,
            state: state.clone(),
        };
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(serving.run(stopped));
        Ok(Self { stop, task, state })
    }

    pub fn faults_served(&self) -> u64 {
        self.state.served.load(Ordering::Relaxed)
    }

    pub fn write_protecting(&self) -> bool {
        self.state.write_protect.load(Ordering::Relaxed)
    }

    pub fn removed_pages(&self) -> RoaringBitmap {
        self.state.removed.lock().clone()
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn stop(self) -> Result<(), String> {
        let _ = self.stop.send(());
        self.task.await.map_err(|e| format!("memory server task: {e}"))?
    }
}

impl Serving {
    async fn run(self, mut stopped: oneshot::Receiver<()>) -> Result<(), String> {
        let mut in_flight = FuturesUnordered::new();
        let mut retries = Vec::new();
        loop {
            let room = in_flight.len() < MAX_FAULTS_IN_FLIGHT;
            tokio::select! {
                _ = &mut stopped => return Ok(()),
                events = self.next_events(!retries.is_empty()), if room => {
                    let faults = match events {
                        Ok(events) => self.record_removals(events),
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                        Err(e) => return Err(format!("read uffd events: {e}")),
                    };
                    in_flight.extend(retries.drain(..).chain(faults).map(|fault| self.serve_fault(fault)));
                }
                Some(served) = in_flight.next() => retries.extend(served?),
            }
        }
    }

    async fn next_events(&self, retrying: bool) -> io::Result<Vec<Event>> {
        if retrying {
            tokio::time::sleep(RETRY_DELAY).await;
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
                Event::PageFault { address, write } => Some(Fault { address, write }),
                Event::Remove { start, end } => {
                    self.mark_removed(start, end);
                    None
                }
            })
            .collect()
    }

    fn mark_removed(&self, start: u64, end: u64) {
        let page_size = self.regions.page_size();
        let pages = (start..end)
            .step_by(page_size as usize)
            .filter_map(|address| self.regions.offset_of(address).ok())
            .map(|offset| (offset / page_size) as u32);
        self.state.removed.lock().extend(pages);
    }

    async fn serve_fault(&self, fault: Fault) -> Result<Option<Fault>, String> {
        let page = fault.address & !(self.regions.page_size() - 1);
        self.install(page, fault.write)
            .await
            .map_err(|e| format!("serve page at {page:#x}: {e}"))
            .map(|outcome| matches!(outcome, CopyOutcome::Retry).then_some(fault))
    }

    async fn install(&self, page: u64, write: bool) -> Result<CopyOutcome, String> {
        let offset = self.regions.offset_of(page)?;
        let bytes = self.page_bytes(offset).await?;
        let protect = !write && self.state.write_protect.load(Ordering::Relaxed);
        let outcome = match self.copy(page, &bytes, protect)? {
            CopyOutcome::NoWriteProtect => {
                self.state.write_protect.store(false, Ordering::Relaxed);
                self.copy(page, &bytes, false)?
            }
            outcome => outcome,
        };
        self.after_copy(page, outcome)?;
        Ok(outcome)
    }

    fn copy(&self, page: u64, bytes: &[u8], protect: bool) -> Result<CopyOutcome, String> {
        self.uffd
            .get_ref()
            .copy(page, bytes, protect)
            .map_err(|e| e.to_string())
    }

    fn after_copy(&self, page: u64, outcome: CopyOutcome) -> Result<(), String> {
        match outcome {
            CopyOutcome::Copied => {
                self.state.served.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            CopyOutcome::AlreadyPresent => self
                .uffd
                .get_ref()
                .wake(page, self.regions.page_size())
                .map_err(|e| e.to_string()),
            CopyOutcome::Retry | CopyOutcome::Gone | CopyOutcome::NoWriteProtect => Ok(()),
        }
    }

    async fn page_bytes(&self, offset: u64) -> Result<Vec<u8>, String> {
        let page_size = self.regions.page_size();
        let mut bytes = vec![0; page_size as usize];
        let removed = self.state.removed.lock().contains((offset / page_size) as u32);
        if !removed {
            self.memfile.read_at(offset, &mut bytes).await?;
        }
        Ok(bytes)
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
        let server = MemoryServer::start(uffd, regions, memfile).unwrap();
        Fixture {
            _dir: dir,
            pages,
            server,
        }
    }

    async fn read_page(pages: &Arc<Pages>, page: u64) -> u8 {
        let address = pages.address() + page * PAGE + 11;
        tokio::task::spawn_blocking(move || Pages::read_byte(address))
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
        let reader = std::thread::spawn(move || Pages::read_byte(address));

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
        let fixture = fixture_with(2, 2, true).await;

        assert_eq!(read_page(&fixture.pages, 0).await, 1);

        assert!(fixture.server.write_protecting());
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

        assert_eq!(fixture.server.removed_pages().iter().collect::<Vec<_>>(), vec![1]);
        fixture.server.stop().await.unwrap();
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
        let server = MemoryServer::start(pages.register().unwrap(), regions, memfile).unwrap();

        let stuck_address = pages.address();
        let stuck = std::thread::spawn(move || Pages::read_byte(stuck_address));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let fast_page = tokio::time::timeout(Duration::from_secs(2), read_page(&pages, 1)).await;
        let stopped = tokio::time::timeout(Duration::from_secs(2), server.stop()).await;
        drop(std::fs::OpenOptions::new().write(true).open(&slow_object));
        let _ = stuck.join();

        assert_eq!(fast_page.ok(), Some(7), "a fault behind a slow page was not served");
        assert!(stopped.is_ok(), "stop waited for a slow page");
    }
}
