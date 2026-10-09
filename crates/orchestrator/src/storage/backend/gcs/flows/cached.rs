use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex as AsyncMutex;

use super::{FlowToken, TokenFlow};

const EXPIRE_EARLY: Duration = Duration::from_secs(225);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Fresh,
    Stale,
    Invalid,
}

#[derive(Default)]
struct Cache {
    token: Option<FlowToken>,
    refresh_running: bool,
    refresh_failed: bool,
}

struct Shared {
    flow: Arc<dyn TokenFlow>,
    cache: std::sync::Mutex<Cache>,
    blocking: AsyncMutex<()>,
}

pub struct CachedFlow {
    shared: Arc<Shared>,
}

fn token_state(token: Option<&FlowToken>, now: SystemTime) -> State {
    match token {
        Some(token) if !token.value.is_empty() => expiry_state(token.expiry, now),
        _ => State::Invalid,
    }
}

fn expiry_state(expiry: Option<SystemTime>, now: SystemTime) -> State {
    match expiry {
        None => State::Fresh,
        Some(expiry) if now > expiry => State::Invalid,
        Some(expiry)
            if expiry
                .checked_sub(EXPIRE_EARLY)
                .is_some_and(|stale_from| now > stale_from) =>
        {
            State::Stale
        }
        Some(_) => State::Fresh,
    }
}

impl Shared {
    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn snapshot(&self) -> (State, Option<FlowToken>) {
        let cache = self.cache();
        (
            token_state(cache.token.as_ref(), SystemTime::now()),
            cache.token.clone(),
        )
    }

    fn begin_refresh(&self) -> bool {
        let mut cache = self.cache();
        let allowed = !cache.refresh_running && !cache.refresh_failed;
        cache.refresh_running |= allowed;
        allowed
    }

    fn start_refresh(self: &Arc<Self>) {
        if !self.begin_refresh() {
            return;
        }
        let shared = self.clone();
        tokio::spawn(async move {
            let fetched = shared.flow.token().await;
            let mut cache = shared.cache();
            cache.refresh_running = false;
            match fetched {
                Ok(token) => cache.token = Some(token),
                Err(_) => cache.refresh_failed = true,
            }
        });
    }

    async fn blocking_token(&self) -> Result<FlowToken, String> {
        let _only_fetcher = self.blocking.lock().await;
        self.cache().refresh_failed = false;
        if let (State::Fresh, Some(token)) = self.snapshot() {
            return Ok(token);
        }
        let fetched = self.flow.token().await?;
        self.cache().token = Some(fetched.clone());
        Ok(fetched)
    }
}

impl CachedFlow {
    pub fn new(flow: Arc<dyn TokenFlow>) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                flow,
                cache: std::sync::Mutex::default(),
                blocking: AsyncMutex::new(()),
            }),
        })
    }
}

#[async_trait::async_trait]
impl TokenFlow for CachedFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        match self.shared.snapshot() {
            (State::Fresh, Some(token)) => Ok(token),
            (State::Stale, Some(token)) => {
                self.shared.start_refresh();
                Ok(token)
            }
            _ => self.shared.blocking_token().await,
        }
    }

    fn universe_domain(&self) -> String {
        self.shared.flow.universe_domain()
    }

    fn is_compute(&self) -> bool {
        self.shared.flow.is_compute()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counting {
        calls: Arc<AtomicUsize>,
        lifetime: Option<Duration>,
        fail_after: usize,
    }

    #[async_trait::async_trait]
    impl TokenFlow for Counting {
        async fn token(&self) -> Result<FlowToken, String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call >= self.fail_after {
                return Err("refresh failed".to_owned());
            }
            Ok(FlowToken {
                value: format!("token-{call}"),
                expiry: self.lifetime.map(|lifetime| SystemTime::now() + lifetime),
            })
        }
    }

    fn cached(lifetime: Option<Duration>, fail_after: usize) -> (Arc<CachedFlow>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let flow = Counting {
            calls: calls.clone(),
            lifetime,
            fail_after,
        };
        (CachedFlow::new(Arc::new(flow)), calls)
    }

    #[tokio::test]
    async fn fresh_and_non_expiring_tokens_are_reused() {
        let (flow, calls) = cached(Some(Duration::from_secs(3600)), usize::MAX);
        let (forever, forever_calls) = cached(None, usize::MAX);

        for _ in 0..3 {
            assert_eq!(flow.token().await.unwrap().value, "token-0");
            assert_eq!(forever.token().await.unwrap().value, "token-0");
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(forever_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stale_token_is_served_while_a_background_refresh_replaces_it() {
        let (flow, calls) = cached(Some(Duration::from_secs(60)), usize::MAX);

        assert_eq!(flow.token().await.unwrap().value, "token-0");
        assert_eq!(flow.token().await.unwrap().value, "token-0");
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if calls.load(Ordering::SeqCst) >= 2 {
                break;
            }
        }

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(flow.token().await.unwrap().value, "token-1");
    }

    #[tokio::test]
    async fn a_failed_background_refresh_keeps_serving_the_stale_token_without_retrying() {
        let (flow, calls) = cached(Some(Duration::from_secs(60)), 1);

        flow.token().await.unwrap();
        for _ in 0..5 {
            assert_eq!(flow.token().await.unwrap().value, "token-0");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_first_error_is_returned_to_the_caller() {
        let (flow, _) = cached(None, 0);

        assert_eq!(flow.token().await.unwrap_err(), "refresh failed");
    }

    #[test]
    fn token_states_follow_the_expiry_windows() {
        let now = SystemTime::now();
        let token = |expiry| FlowToken {
            value: "t".to_owned(),
            expiry,
        };

        assert_eq!(token_state(None, now), State::Invalid);
        assert_eq!(token_state(Some(&token(None)), now), State::Fresh);
        assert_eq!(
            token_state(Some(&token(Some(now + Duration::from_secs(3600)))), now),
            State::Fresh
        );
        assert_eq!(
            token_state(Some(&token(Some(now + Duration::from_secs(224)))), now),
            State::Stale
        );
        assert_eq!(
            token_state(Some(&token(Some(now - Duration::from_secs(1)))), now),
            State::Invalid
        );
    }
}
