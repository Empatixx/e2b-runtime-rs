use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

#[derive(Debug)]
struct Usage {
    used: i64,
    limit: i64,
}

#[derive(Debug)]
pub struct AdjustableSemaphore {
    usage: Mutex<Usage>,
    changed: Notify,
}

#[derive(Debug)]
pub struct StartPermit {
    semaphore: Arc<AdjustableSemaphore>,
}

fn positive_limit(limit: i64, context: &str) -> Result<i64, String> {
    match limit > 0 {
        true => Ok(limit),
        false => Err(format!("{context}: limit must be > 0, got: {limit}")),
    }
}

impl AdjustableSemaphore {
    pub fn new(limit: i64) -> Result<Arc<Self>, String> {
        positive_limit(limit, "NewAdjustableSemaphore").map(|limit| {
            Arc::new(Self {
                usage: Mutex::new(Usage { used: 0, limit }),
                changed: Notify::new(),
            })
        })
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<StartPermit> {
        let mut usage = self.usage.lock();
        (usage.used < usage.limit).then(|| {
            usage.used += 1;
            StartPermit {
                semaphore: self.clone(),
            }
        })
    }

    pub async fn acquire(self: &Arc<Self>) -> StartPermit {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(permit) = self.try_acquire() {
                return permit;
            }
            changed.await;
        }
    }

    pub fn set_limit(&self, limit: i64) -> Result<(), String> {
        let limit = positive_limit(limit, "SetLimit")?;
        let mut usage = self.usage.lock();
        if usage.limit != limit {
            usage.limit = limit;
            self.changed.notify_waiters();
        }
        Ok(())
    }

    pub fn limit(&self) -> i64 {
        self.usage.lock().limit
    }
}

impl Drop for StartPermit {
    fn drop(&mut self) {
        self.semaphore.usage.lock().used -= 1;
        self.semaphore.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_limit_must_be_positive() {
        assert_eq!(
            AdjustableSemaphore::new(0).unwrap_err(),
            "NewAdjustableSemaphore: limit must be > 0, got: 0"
        );
        let semaphore = AdjustableSemaphore::new(1).unwrap();
        assert_eq!(
            semaphore.set_limit(-2),
            Err("SetLimit: limit must be > 0, got: -2".to_owned())
        );
        assert_eq!(semaphore.limit(), 1);
    }

    #[test]
    fn try_acquire_stops_at_the_limit_and_a_dropped_permit_frees_a_slot() {
        let semaphore = AdjustableSemaphore::new(2).unwrap();

        let first = semaphore.try_acquire();
        let second = semaphore.try_acquire();

        assert!(first.is_some() && second.is_some());
        assert!(semaphore.try_acquire().is_none());
        drop(first);
        assert!(semaphore.try_acquire().is_some());
    }

    #[test]
    fn lowering_the_limit_below_the_used_count_blocks_new_permits_without_revoking() {
        let semaphore = AdjustableSemaphore::new(3).unwrap();
        let held: Vec<_> = (0..3).filter_map(|_| semaphore.try_acquire()).collect();

        semaphore.set_limit(1).unwrap();

        assert_eq!(held.len(), 3);
        assert!(semaphore.try_acquire().is_none());
        drop(held);
        assert!(semaphore.try_acquire().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_wakes_when_the_limit_grows() {
        let semaphore = AdjustableSemaphore::new(1).unwrap();
        let held = semaphore.try_acquire().unwrap();
        let waiter = tokio::spawn({
            let semaphore = semaphore.clone();
            async move { semaphore.acquire().await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!waiter.is_finished());

        semaphore.set_limit(2).unwrap();

        assert!(waiter.await.is_ok());
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_wakes_when_a_permit_is_released() {
        let semaphore = AdjustableSemaphore::new(1).unwrap();
        let held = semaphore.try_acquire().unwrap();
        let waiter = tokio::spawn({
            let semaphore = semaphore.clone();
            async move { semaphore.acquire().await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;

        drop(held);

        assert!(waiter.await.is_ok());
    }
}
