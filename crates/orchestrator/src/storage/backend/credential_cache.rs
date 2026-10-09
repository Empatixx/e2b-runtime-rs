use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

const REFRESH_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Entry<T> {
    value: Arc<T>,
    expires_at: Option<SystemTime>,
}

impl<T> Entry<T> {
    fn is_fresh(&self) -> bool {
        self.expires_at
            .is_none_or(|expiry| expiry > SystemTime::now() + REFRESH_MARGIN)
    }
}

#[derive(Debug)]
pub struct CredentialCache<T> {
    slot: Mutex<Option<Entry<T>>>,
}

impl<T> Default for CredentialCache<T> {
    fn default() -> Self {
        Self { slot: Mutex::new(None) }
    }
}

impl<T> CredentialCache<T> {
    pub async fn get_or_fetch<F, Fut>(&self, fetch: F) -> object_store::Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = object_store::Result<(T, Option<SystemTime>)>>,
    {
        let mut slot = self.slot.lock().await;
        if let Some(entry) = slot.as_ref().filter(|entry| entry.is_fresh()) {
            return Ok(entry.value.clone());
        }
        let (value, expires_at) = fetch().await?;
        let value = Arc::new(value);
        *slot = Some(Entry {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    async fn fetch_counted(cache: &CredentialCache<u32>, calls: &AtomicUsize, expires_in: Option<Duration>) -> u32 {
        *cache
            .get_or_fetch(|| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((7, expires_in.map(|ttl| SystemTime::now() + ttl)))
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fresh_values_are_reused_and_expiring_ones_refetched() {
        let cache = CredentialCache::default();
        let calls = AtomicUsize::new(0);

        fetch_counted(&cache, &calls, Some(Duration::from_secs(3600))).await;
        fetch_counted(&cache, &calls, Some(Duration::from_secs(3600))).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let expiring = CredentialCache::default();
        fetch_counted(&expiring, &calls, Some(Duration::from_secs(10))).await;
        fetch_counted(&expiring, &calls, Some(Duration::from_secs(10))).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let unlimited = CredentialCache::default();
        fetch_counted(&unlimited, &calls, None).await;
        fetch_counted(&unlimited, &calls, None).await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
