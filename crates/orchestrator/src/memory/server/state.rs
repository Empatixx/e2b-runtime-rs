use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use roaring::RoaringBitmap;

use super::write_protect::WriteProtect;
use crate::memory::trace::{Access, PrefetchTracker, Trace};
use crate::memory::uffd::{CopyOutcome, Uffd};

pub struct Gate {
    pub closed: bool,
}

pub struct SharedState {
    served: AtomicU64,
    write_protect: WriteProtect,
    removed: Mutex<RoaringBitmap>,
    installed: Mutex<RoaringBitmap>,
    trace: Mutex<PrefetchTracker>,
    gate: RwLock<Gate>,
    failure: Mutex<Option<String>>,
}

impl SharedState {
    pub fn new(page_size: u64) -> Self {
        Self {
            served: AtomicU64::new(0),
            write_protect: WriteProtect::default(),
            removed: Mutex::default(),
            installed: Mutex::default(),
            trace: Mutex::new(PrefetchTracker::new(page_size)),
            gate: RwLock::new(Gate { closed: false }),
            failure: Mutex::default(),
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

    pub fn served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    pub fn count_served(&self) {
        self.served.fetch_add(1, Ordering::Relaxed);
    }

    pub fn write_protecting(&self) -> bool {
        self.write_protect.confirmed()
    }

    pub fn trace(&self) -> Trace {
        self.trace.lock().collect()
    }

    pub fn touched(&self, page: u32, access: Access) {
        self.installed.lock().insert(page);
        self.trace.lock().record(u64::from(page), access);
    }

    pub fn is_untouched(&self, page: u32) -> bool {
        !self.installed.lock().contains(page) && !self.removed.lock().contains(page)
    }

    pub fn is_removed(&self, page: u32) -> bool {
        self.removed.lock().contains(page)
    }

    pub fn mark_removed(&self, pages: impl IntoIterator<Item = u32>) {
        let _batch = self.gate.write();
        self.removed.lock().extend(pages);
    }

    pub fn removed(&self) -> RoaringBitmap {
        self.removed.lock().clone()
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
        let state = SharedState::new(4096);

        state.touched(1, Access::Read);
        state.mark_removed([2]);

        assert!(state.is_untouched(0));
        assert!(!state.is_untouched(1));
        assert!(!state.is_untouched(2));
        assert_eq!(state.removed().iter().collect::<Vec<_>>(), vec![2]);
        assert!(state.is_removed(2));
    }

    #[test]
    fn closing_is_seen_by_later_installs() {
        let state = SharedState::new(4096);
        assert!(!state.installing().closed);

        state.close();

        assert!(state.installing().closed);
    }

    #[test]
    fn a_removal_waits_for_running_installs() {
        let state = std::sync::Arc::new(SharedState::new(4096));
        let installing = state.installing();
        let removal = std::thread::spawn({
            let state = state.clone();
            move || state.mark_removed([7])
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!state.is_removed(7));
        drop(installing);
        removal.join().unwrap();

        assert!(state.is_removed(7));
    }
}
