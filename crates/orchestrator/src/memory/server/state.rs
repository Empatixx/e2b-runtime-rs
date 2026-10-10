use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::{Mutex, RwLock, RwLockReadGuard};

use super::counters::{ServeCounters, ServeStats};
use super::wp_failures::WpFailures;
use super::write_protect::WriteProtect;
use crate::memory::balloon_mode::BalloonMode;
use crate::memory::cow_window::CowWindow;
use crate::memory::page_tracker::{PageState, PageTracker};
use crate::memory::trace::{Access, PrefetchTracker, Trace};
use crate::memory::uffd::{CopyOutcome, Uffd};
use crate::telemetry::uffd::{GenerationBucket, record_cow_cancel};

pub struct Gate {
    pub closed: bool,
}

pub struct SharedState {
    pub counters: ServeCounters,
    pub wp_failures: WpFailures,
    pub tracker: Arc<PageTracker>,
    pub generation: GenerationBucket,
    write_protect: WriteProtect,
    trace: Mutex<PrefetchTracker>,
    gate: RwLock<Gate>,
    failure: Mutex<Option<String>>,
    cow: RwLock<Option<Arc<CowWindow>>>,
    mode: AtomicU8,
}

impl SharedState {
    pub fn new(page_size: u64, generation: u64) -> Self {
        Self {
            counters: ServeCounters::default(),
            wp_failures: WpFailures::default(),
            tracker: Arc::default(),
            generation: GenerationBucket::of(generation),
            write_protect: WriteProtect::default(),
            trace: Mutex::new(PrefetchTracker::new(page_size)),
            gate: RwLock::new(Gate { closed: false }),
            failure: Mutex::default(),
            cow: RwLock::new(None),
            mode: AtomicU8::new(BalloonMode::Unknown.raw()),
        }
    }

    pub fn installing(&self) -> RwLockReadGuard<'_, Gate> {
        self.gate.read()
    }

    pub fn close(&self) {
        self.gate.write().closed = true;
    }

    pub fn fail(&self, error: String) {
        *self.failure.lock() = Some(error);
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().clone()
    }

    pub fn stats(&self) -> ServeStats {
        self.counters.snapshot()
    }

    pub fn write_protecting(&self) -> bool {
        self.write_protect.confirmed()
    }

    pub fn balloon_mode(&self) -> BalloonMode {
        BalloonMode::from_raw(self.mode.load(Ordering::Relaxed))
    }

    pub fn set_balloon_mode(&self, mode: BalloonMode) {
        self.mode.store(mode.raw(), Ordering::Relaxed);
    }

    pub fn trace(&self) -> Trace {
        self.trace.lock().collect()
    }

    pub fn installed(&self, page: u32, access: Access, zero_filled: bool) {
        let pages = page..page + 1;
        match (access, zero_filled) {
            (Access::Write, _) => self.tracker.set_range(pages, PageState::Dirty),
            (_, false) => self.tracker.mark_installed(pages, PageState::Clean),
            (_, true) => self.tracker.mark_installed(pages, PageState::Zero),
        }
        self.trace.lock().record(u64::from(page), access);
    }

    pub fn trace_prefetch(&self, page: u32) {
        self.trace.lock().record(u64::from(page), Access::Prefetch);
    }

    pub fn page_state(&self, page: u32) -> PageState {
        self.tracker.state(page)
    }

    pub fn is_untouched(&self, page: u32) -> bool {
        self.tracker.state(page) == PageState::NotPresent
    }

    pub fn mark_removed(&self, pages: std::ops::Range<u32>) {
        let _batch = self.gate.write();
        self.tracker.set_range(pages.clone(), PageState::Removed);
        if let Some(window) = self
            .cow
            .read()
            .as_ref()
            .filter(|window| window.overlaps(pages.start, pages.end))
        {
            tracing::error!(
                start_idx = pages.start,
                end_idx = pages.end,
                "uffd REMOVE overlaps CoW-window pages; canceling export"
            );
            record_cow_cancel("remove_tripwire");
            window.cancel(format!("REMOVE zapped window pages [{}, {})", pages.start, pages.end));
        }
    }

    pub fn cow_window(&self) -> Option<Arc<CowWindow>> {
        self.cow.read().clone()
    }

    pub fn install_cow_window(&self, window: Arc<CowWindow>) {
        *self.cow.write() = Some(window);
    }

    pub fn uninstall_cow_window(&self, window: &Arc<CowWindow>) {
        let mut installed = self.cow.write();
        if installed.as_ref().is_some_and(|current| Arc::ptr_eq(current, window)) {
            *installed = None;
        }
    }

    pub fn take_cow_window(&self) -> Option<Arc<CowWindow>> {
        self.cow.write().take()
    }

    pub fn copy_page(&self, uffd: &Uffd, address: u64, bytes: &[u8], read: bool) -> Result<CopyOutcome, String> {
        let protect = read && self.write_protect.requested();
        match copy(uffd, address, bytes, protect)? {
            CopyOutcome::NoWriteProtect => {
                self.write_protect.disable();
                copy(uffd, address, bytes, false)
            }
            CopyOutcome::Copied if protect => {
                self.write_protect.confirm();
                Ok(CopyOutcome::Copied)
            }
            outcome => Ok(outcome),
        }
    }
}

fn copy(uffd: &Uffd, address: u64, bytes: &[u8], protect: bool) -> Result<CopyOutcome, String> {
    uffd.copy(address, bytes, protect).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_and_installed_pages_are_not_untouched() {
        let state = SharedState::new(4096, 0);

        state.installed(1, Access::Read, false);
        state.mark_removed(2..3);

        assert!(state.is_untouched(0));
        assert!(!state.is_untouched(1));
        assert!(!state.is_untouched(2));
        assert_eq!(
            state.tracker.export_states().removed.iter().collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(state.page_state(2), PageState::Removed);
    }

    #[test]
    fn closing_is_seen_by_later_installs() {
        let state = SharedState::new(4096, 0);
        assert!(!state.installing().closed);

        state.close();

        assert!(state.installing().closed);
    }

    #[test]
    fn a_removal_waits_for_running_installs() {
        let state = std::sync::Arc::new(SharedState::new(4096, 0));
        let installing = state.installing();
        let removal = std::thread::spawn({
            let state = state.clone();
            move || state.mark_removed(7..8)
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_ne!(state.page_state(7), PageState::Removed);
        drop(installing);
        removal.join().unwrap();

        assert_eq!(state.page_state(7), PageState::Removed);
    }
}
