use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex as AsyncMutex;

const REFRESH_MARGIN: Duration = Duration::from_secs(60);

struct Entry<T> {
    value: Arc<T>,
    expires_at: Option<SystemTime>,
}

impl<T> Entry<T> {
    fn is_fresh(&self) -> bool {
        self.expires_at
            .is_none_or(|expiry| expiry > SystemTime::now() + REFRESH_MARGIN)
    }

    fn is_unexpired(&self) -> bool {
        self.expires_at.is_none_or(|expiry| expiry > SystemTime::now())
    }
}

pub struct CredentialCache<T> {
    slot: Mutex<Option<Entry<T>>>,
    fetching: AsyncMutex<()>,
}

impl<T> std::fmt::Debug for CredentialCache<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialCache").finish_non_exhaustive()
    }
}

impl<T> Default for CredentialCache<T> {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            fetching: AsyncMutex::new(()),
        }
    }
}

impl<T> CredentialCache<T> {
    pub async fn get_or_fetch<F, Fut>(&self, fetch: F) -> object_store::Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = object_store::Result<(T, Option<SystemTime>)>>,
    {
        if let Some(value) = self.value_where(Entry::is_fresh) {
            return Ok(value);
        }
        match self.fetching.try_lock() {
            Ok(_fetcher) => self.refill(fetch).await,
            Err(_) => match self.value_where(Entry::is_unexpired) {
                Some(value) => Ok(value),
                None => self.wait_then_refill(fetch).await,
            },
        }
    }

    async fn wait_then_refill<F, Fut>(&self, fetch: F) -> object_store::Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = object_store::Result<(T, Option<SystemTime>)>>,
    {
        let _fetcher = self.fetching.lock().await;
        match self.value_where(Entry::is_fresh) {
            Some(value) => Ok(value),
            None => self.refill(fetch).await,
        }
    }

    async fn refill<F, Fut>(&self, fetch: F) -> object_store::Result<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = object_store::Result<(T, Option<SystemTime>)>>,
    {
        let (value, expires_at) = fetch().await?;
        let value = Arc::new(value);
        *self.slot() = Some(Entry {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }

    fn value_where(&self, accepts: impl Fn(&Entry<T>) -> bool) -> Option<Arc<T>> {
        self.slot()
            .as_ref()
            .filter(|entry| accepts(entry))
            .map(|entry| entry.value.clone())
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Entry<T>>> {
        self.slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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

    #[tokio::test]
    async fn a_slow_refresh_does_not_block_callers_while_the_old_value_is_still_valid() {
        let cache = Arc::new(CredentialCache::default());
        cache
            .get_or_fetch(|| async { Ok((1u32, Some(SystemTime::now() + Duration::from_secs(30)))) })
            .await
            .unwrap();
        let slow = tokio::spawn({
            let cache = cache.clone();
            async move {
                cache
                    .get_or_fetch(|| async {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        Ok((2u32, None))
                    })
                    .await
            }
        });
        tokio::task::yield_now().await;

        let meanwhile = tokio::time::timeout(
            Duration::from_secs(1),
            cache.get_or_fetch(|| async { Ok((3u32, None)) }),
        )
        .await;

        assert_eq!(*meanwhile.unwrap().unwrap(), 1);
        slow.abort();
    }

    #[tokio::test]
    async fn concurrent_callers_without_a_usable_value_share_one_fetch() {
        let cache = Arc::new(CredentialCache::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (cache, calls) = (cache.clone(), calls.clone());
                tokio::spawn(async move {
                    cache
                        .get_or_fetch(|| async {
                            calls.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            Ok((9u32, None))
                        })
                        .await
                        .map(|value| *value)
                })
            })
            .collect();

        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), 9);
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn debug_output_never_shows_cached_credentials() {
        let cache = CredentialCache::default();
        cache
            .get_or_fetch(|| async { Ok(("bearer-secret".to_owned(), None)) })
            .await
            .unwrap();

        assert!(!format!("{cache:?}").contains("bearer-secret"));
    }
}
