use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub struct RateLimit {
    floor: Duration,
    last: Mutex<Option<Instant>>,
    suppressed: AtomicI64,
}

impl RateLimit {
    pub const fn new(floor: Duration) -> Self {
        Self {
            floor,
            last: Mutex::new(None),
            suppressed: AtomicI64::new(0),
        }
    }

    pub fn allow(&self) -> Option<i64> {
        let mut last = self.last.lock();
        match *last {
            Some(logged) if logged.elapsed() <= self.floor => {
                self.suppressed.fetch_add(1, Ordering::SeqCst);
                None
            }
            _ => {
                *last = Some(Instant::now());
                Some(self.suppressed.swap(0, Ordering::SeqCst))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_once_per_floor_and_counts_the_rest() {
        let limit = RateLimit::new(Duration::from_secs(10));
        assert_eq!(limit.allow(), Some(0));
        assert_eq!(limit.allow(), None);
        assert_eq!(limit.allow(), None);
        *limit.last.lock() = Some(Instant::now() - Duration::from_secs(11));
        assert_eq!(limit.allow(), Some(2));
    }
}
