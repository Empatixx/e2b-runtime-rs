use std::collections::HashMap;

use parking_lot::Mutex;

#[derive(Default)]
pub struct ConnectionLimiter {
    counts: Mutex<HashMap<String, i64>>,
}

impl ConnectionLimiter {
    pub fn try_acquire(&self, key: &str, max_limit: i64) -> (i64, bool) {
        match max_limit {
            0 => (self.count(key), false),
            _ => self.acquire_within(key, max_limit),
        }
    }

    pub fn release(&self, key: &str) {
        let mut counts = self.counts.lock();
        let emptied = counts.get_mut(key).map(|count| {
            *count = (*count - 1).max(0);
            *count == 0
        });
        if emptied == Some(true) {
            counts.remove(key);
        }
    }

    pub fn remove(&self, key: &str) {
        self.counts.lock().remove(key);
    }

    pub fn count(&self, key: &str) -> i64 {
        self.counts.lock().get(key).copied().unwrap_or(0)
    }

    fn acquire_within(&self, key: &str, max_limit: i64) -> (i64, bool) {
        let mut counts = self.counts.lock();
        let count = counts.entry(key.to_owned()).or_insert(0);
        let acquired = max_limit < 0 || *count < max_limit;
        *count += i64::from(acquired);
        (*count, acquired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquires_up_to_limit() {
        let limiter = ConnectionLimiter::default();
        assert_eq!(limiter.try_acquire("sandbox1", 3), (1, true));
        assert_eq!(limiter.try_acquire("sandbox1", 3), (2, true));
        assert_eq!(limiter.try_acquire("sandbox1", 3), (3, true));
    }

    #[test]
    fn rejects_when_at_limit() {
        let limiter = ConnectionLimiter::default();
        (0..5).for_each(|_| {
            limiter.try_acquire("sandbox1", 5);
        });
        assert_eq!(limiter.try_acquire("sandbox1", 5), (5, false));
    }

    #[test]
    fn zero_limit_blocks_all_connections() {
        let limiter = ConnectionLimiter::default();
        assert_eq!(limiter.try_acquire("sandbox1", 0), (0, false));
        assert_eq!(limiter.count("sandbox1"), 0);
    }

    #[test]
    fn negative_limit_means_no_limit() {
        let limiter = ConnectionLimiter::default();
        (0..100).for_each(|_| assert!(limiter.try_acquire("sandbox1", -1).1));
        assert_eq!(limiter.count("sandbox1"), 100);
    }

    #[test]
    fn separate_sandboxes_have_separate_limits() {
        let limiter = ConnectionLimiter::default();
        (0..3).for_each(|_| {
            limiter.try_acquire("sandbox1", 3);
        });
        assert_eq!(limiter.try_acquire("sandbox2", 3), (1, true));
    }

    #[test]
    fn release_allows_new_acquire() {
        let limiter = ConnectionLimiter::default();
        (0..3).for_each(|_| {
            limiter.try_acquire("sandbox1", 3);
        });
        assert!(!limiter.try_acquire("sandbox1", 3).1);
        limiter.release("sandbox1");
        assert!(limiter.try_acquire("sandbox1", 3).1);
    }

    #[test]
    fn release_on_unknown_sandbox_is_safe() {
        ConnectionLimiter::default().release("unknown");
    }

    #[test]
    fn double_release_does_not_go_negative() {
        let limiter = ConnectionLimiter::default();
        limiter.try_acquire("sandbox1", 10);
        assert_eq!(limiter.count("sandbox1"), 1);
        (0..3).for_each(|_| {
            limiter.release("sandbox1");
            assert_eq!(limiter.count("sandbox1"), 0);
        });
    }

    #[test]
    fn remove_forgets_the_key() {
        let limiter = ConnectionLimiter::default();
        limiter.try_acquire("sandbox1", 10);
        limiter.remove("sandbox1");
        assert_eq!(limiter.count("sandbox1"), 0);
    }
}
