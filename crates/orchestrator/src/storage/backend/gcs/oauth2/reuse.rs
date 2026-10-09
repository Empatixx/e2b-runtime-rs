use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use super::super::flows::{FlowToken, TokenFlow};

pub const DEFAULT_EXPIRY_DELTA: Duration = Duration::from_secs(10);

pub struct ReuseFlow {
    flow: Arc<dyn TokenFlow>,
    expiry_delta: Duration,
    current: Mutex<Option<FlowToken>>,
}

impl ReuseFlow {
    pub fn new(flow: Arc<dyn TokenFlow>) -> Arc<Self> {
        Self::with_expiry_delta(flow, DEFAULT_EXPIRY_DELTA)
    }

    pub fn with_expiry_delta(flow: Arc<dyn TokenFlow>, expiry_delta: Duration) -> Arc<Self> {
        Arc::new(Self {
            flow,
            expiry_delta,
            current: Mutex::new(None),
        })
    }

    fn is_valid(&self, token: &FlowToken) -> bool {
        !token.value.is_empty()
            && token.expiry.is_none_or(|expiry| {
                expiry
                    .checked_sub(self.expiry_delta)
                    .is_none_or(|valid_until| valid_until >= SystemTime::now())
            })
    }
}

#[async_trait::async_trait]
impl TokenFlow for ReuseFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        let mut current = self.current.lock().await;
        if let Some(token) = current.as_ref().filter(|token| self.is_valid(token)) {
            return Ok(token.clone());
        }
        let fetched = self.flow.token().await?;
        *current = Some(fetched.clone());
        Ok(fetched)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counting {
        calls: Arc<AtomicUsize>,
        value: &'static str,
        lifetime: Option<Duration>,
    }

    #[async_trait::async_trait]
    impl TokenFlow for Counting {
        async fn token(&self) -> Result<FlowToken, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(FlowToken {
                value: self.value.to_owned(),
                expiry: self.lifetime.map(|lifetime| SystemTime::now() + lifetime),
            })
        }
    }

    fn reuse(value: &'static str, lifetime: Option<Duration>, delta: Duration) -> (Arc<ReuseFlow>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let flow = Counting {
            calls: calls.clone(),
            value,
            lifetime,
        };
        (ReuseFlow::with_expiry_delta(Arc::new(flow), delta), calls)
    }

    #[tokio::test]
    async fn valid_tokens_are_reused_until_the_expiry_delta() {
        let (valid, valid_calls) = reuse("t", Some(Duration::from_secs(3600)), DEFAULT_EXPIRY_DELTA);
        let (never, never_calls) = reuse("t", None, DEFAULT_EXPIRY_DELTA);
        let (soon, soon_calls) = reuse("t", Some(Duration::from_secs(5)), DEFAULT_EXPIRY_DELTA);

        for _ in 0..3 {
            valid.token().await.unwrap();
            never.token().await.unwrap();
            soon.token().await.unwrap();
        }

        assert_eq!(valid_calls.load(Ordering::SeqCst), 1);
        assert_eq!(never_calls.load(Ordering::SeqCst), 1);
        assert_eq!(soon_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn empty_tokens_are_never_reused() {
        let (empty, calls) = reuse("", None, DEFAULT_EXPIRY_DELTA);

        empty.token().await.unwrap();
        empty.token().await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_larger_delta_refreshes_earlier() {
        let (flow, calls) = reuse("t", Some(Duration::from_secs(100)), Duration::from_secs(225));

        flow.token().await.unwrap();
        flow.token().await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
