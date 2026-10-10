use std::sync::atomic::{AtomicI64, Ordering};

use crate::telemetry::uffd::{FaultResult, PageClass};

#[derive(Debug, Default)]
pub struct ServeCounters {
    pages: AtomicI64,
    source_pages: AtomicI64,
    bytes: AtomicI64,
    deferred: AtomicI64,
    wp_deferred: AtomicI64,
    wp_faults: AtomicI64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServeStats {
    pub pages: i64,
    pub source_pages: i64,
    pub bytes: i64,
    pub wp_faults: i64,
    pub deferred: i64,
    pub wp_deferred: i64,
}

impl ServeCounters {
    pub fn snapshot(&self) -> ServeStats {
        ServeStats {
            pages: self.pages.load(Ordering::Relaxed),
            source_pages: self.source_pages.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            wp_faults: self.wp_faults.load(Ordering::Relaxed),
            deferred: self.deferred.load(Ordering::Relaxed),
            wp_deferred: self.wp_deferred.load(Ordering::Relaxed),
        }
    }

    pub fn record_serve(&self, class: PageClass, result: FaultResult, bytes: i64) {
        match result {
            FaultResult::Installed | FaultResult::Present => {
                self.pages.fetch_add(1, Ordering::Relaxed);
                if bytes > 0 {
                    self.bytes.fetch_add(bytes, Ordering::Relaxed);
                    if class == PageClass::New {
                        self.source_pages.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            FaultResult::Deferred => {
                self.deferred.fetch_add(1, Ordering::Relaxed);
            }
            FaultResult::Discarded | FaultResult::Error | FaultResult::Skipped => {}
        }
    }

    pub fn record_wp_deferred(&self) {
        self.wp_deferred.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_wp_resolved(&self) {
        self.wp_faults.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_attempts_are_counted_on_their_own_and_not_as_needed_pages() {
        let counters = ServeCounters::default();

        counters.record_serve(PageClass::New, FaultResult::Deferred, 0);
        counters.record_serve(PageClass::New, FaultResult::Installed, 4096);
        let stats = counters.snapshot();

        assert_eq!(stats.deferred, 1);
        assert_eq!(stats.pages, 1);
        assert_eq!(stats.bytes, 4096);
        assert_eq!(stats.source_pages, 1);
    }

    #[test]
    fn a_present_page_counts_as_needed_but_adds_no_bytes() {
        let counters = ServeCounters::default();

        counters.record_serve(PageClass::Resident, FaultResult::Present, 0);

        assert_eq!(
            counters.snapshot(),
            ServeStats {
                pages: 1,
                ..ServeStats::default()
            }
        );
    }

    #[test]
    fn zero_fills_add_bytes_but_not_source_pages() {
        let counters = ServeCounters::default();

        counters.record_serve(PageClass::Zero, FaultResult::Installed, 4096);

        let stats = counters.snapshot();
        assert_eq!((stats.bytes, stats.source_pages), (4096, 0));
    }
}
