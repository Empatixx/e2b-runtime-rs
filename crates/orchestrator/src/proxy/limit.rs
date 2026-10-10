use std::sync::Arc;
use std::time::Instant;

use super::limiter::ConnectionLimiter;

type Callback<T> = Arc<dyn Fn(T) + Send + Sync>;

#[derive(Clone, bon::Builder)]
pub struct ConnectionLimit {
    limiter: Arc<ConnectionLimiter>,
    max_limit: Arc<dyn Fn() -> i64 + Send + Sync>,
    on_acquired: Option<Callback<i64>>,
    on_released: Option<Callback<u64>>,
    on_blocked: Option<Callback<()>>,
}

#[derive(Debug)]
pub struct OverLimit {
    pub max_limit: i64,
}

impl ConnectionLimit {
    pub fn acquire(&self, key: &str) -> Result<Permit, OverLimit> {
        let max_limit = (self.max_limit)();
        match self.limiter.try_acquire(key, max_limit) {
            (_, false) => {
                self.on_blocked.iter().for_each(|callback| callback(()));
                Err(OverLimit { max_limit })
            }
            (count, true) => {
                self.on_acquired.iter().for_each(|callback| callback(count));
                Ok(Permit {
                    limit: self.clone(),
                    key: key.to_owned(),
                    started: Instant::now(),
                })
            }
        }
    }
}

pub struct Permit {
    limit: ConnectionLimit,
    key: String,
    started: Instant,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limit.limiter.release(&self.key);
        let elapsed = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.limit.on_released.iter().for_each(|callback| callback(elapsed));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    fn limit_of(max: i64, counters: &Arc<[AtomicI64; 3]>) -> ConnectionLimit {
        let (acquired, released, blocked) = (counters.clone(), counters.clone(), counters.clone());
        ConnectionLimit::builder()
            .limiter(Arc::default())
            .max_limit(Arc::new(move || max))
            .on_acquired(Arc::new(move |_| {
                acquired[0].fetch_add(1, Ordering::SeqCst);
            }))
            .on_released(Arc::new(move |_| {
                released[1].fetch_add(1, Ordering::SeqCst);
            }))
            .on_blocked(Arc::new(move |()| {
                blocked[2].fetch_add(1, Ordering::SeqCst);
            }))
            .build()
    }

    #[test]
    fn blocks_over_the_limit_and_reports_each_event() {
        let counters = Arc::new([AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)]);
        let limit = limit_of(2, &counters);
        let first = limit.acquire("key").unwrap();
        let second = limit.acquire("key").unwrap();
        assert_eq!(limit.acquire("key").err().map(|over| over.max_limit), Some(2));
        assert_eq!(counters[0].load(Ordering::SeqCst), 2);
        assert_eq!(counters[2].load(Ordering::SeqCst), 1);
        drop((first, second));
        assert_eq!(counters[1].load(Ordering::SeqCst), 2);
        assert!(limit.acquire("key").is_ok());
    }
}
