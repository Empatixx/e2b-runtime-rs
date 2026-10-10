use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

pub const ESCALATION: i32 = 8;

#[derive(Debug, Default)]
pub struct WpFailures {
    streaks: Mutex<HashMap<u64, i32>>,
    pending: AtomicUsize,
}

impl WpFailures {
    pub fn record(&self, aligned_address: u64) -> i32 {
        let mut streaks = self.streaks.lock();
        let streak = streaks.entry(aligned_address).or_insert(0);
        *streak += 1;
        let streak = *streak;
        self.pending.store(streaks.len(), Ordering::Release);
        streak
    }

    pub fn clear(&self, aligned_address: u64) {
        if self.pending.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut streaks = self.streaks.lock();
        if streaks.remove(&aligned_address).is_some() {
            self.pending.store(streaks.len(), Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streak_counts_failures_of_the_same_page() {
        let failures = WpFailures::default();

        assert_eq!(failures.record(0x1000), 1);
        assert_eq!(failures.record(0x1000), 2);
        assert_eq!(failures.record(0x2000), 1, "streaks are per page");
    }

    #[test]
    fn a_resolved_page_starts_over() {
        let failures = WpFailures::default();
        failures.record(0x1000);
        failures.record(0x1000);

        failures.clear(0x1000);

        assert_eq!(failures.record(0x1000), 1);
    }

    #[test]
    fn clearing_without_failures_is_a_noop() {
        let failures = WpFailures::default();

        failures.clear(0x1000);

        assert_eq!(failures.record(0x1000), 1);
    }
}
