use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use crate::telemetry::server::in_place::record_fpr_resume;

const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const RETRIES: usize = 4;

pub type Resume = Arc<dyn Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync>;
pub type PauseGeneration = Arc<dyn Fn() -> u64 + Send + Sync>;
pub type Record = Arc<dyn Fn(&str) + Send + Sync>;
pub type Exited = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
pub struct FprResume {
    pub resume: Resume,
    pub firecracker_exited: Exited,
    pub pause_generation: PauseGeneration,
    pub attempt_timeout: Duration,
    pub retry_delay: Duration,
    pub record: Record,
}

impl FprResume {
    pub fn production(resume: Resume, firecracker_exited: Exited, pause_generation: PauseGeneration) -> Self {
        Self {
            resume,
            firecracker_exited,
            pause_generation,
            attempt_timeout: ATTEMPT_TIMEOUT,
            retry_delay: RETRY_DELAY,
            record: Arc::new(record_fpr_resume),
        }
    }

    pub async fn run(&self) {
        let first = self.attempt().await;
        let Err(error) = first else {
            (self.record)("inline");
            return;
        };
        let generation = (self.pause_generation)();
        let this = self.clone();
        tokio::spawn(async move { this.retry(generation, error).await });
    }

    async fn attempt(&self) -> Result<(), String> {
        tokio::time::timeout(self.attempt_timeout, (self.resume)())
            .await
            .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()))
    }

    async fn retry(&self, generation: u64, mut last_error: String) {
        for attempt in 0..RETRIES {
            tokio::time::sleep(self.retry_delay).await;
            if (self.firecracker_exited)() {
                (self.record)("fc_exited");
                return;
            }
            if (self.pause_generation)() != generation {
                (self.record)("fenced");
                return;
            }
            match self.attempt().await {
                Ok(()) => {
                    (self.record)("retry");
                    tracing::info!(attempt = attempt + 2, "free-page reporting resumed after retry");
                    return;
                }
                Err(error) => last_error = error,
            }
        }
        (self.record)("abandoned");
        tracing::error!(
            attempts = RETRIES + 1,
            error = %last_error,
            "resuming free-page reporting failed; reporting stays paused for this sandbox's lifetime"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use parking_lot::Mutex;

    use super::*;

    fn deps(
        resume: impl Fn(u32) -> Result<(), String> + Send + Sync + 'static,
        generation: PauseGeneration,
        exited: Exited,
    ) -> (FprResume, Arc<Mutex<Vec<String>>>, Arc<AtomicU32>) {
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicU32::new(0));
        let counted = calls.clone();
        let recorded = outcomes.clone();
        let fpr = FprResume {
            resume: Arc::new(move || {
                let call = counted.fetch_add(1, Ordering::SeqCst) + 1;
                let outcome = resume(call);
                Box::pin(async move { outcome })
            }),
            firecracker_exited: exited,
            pause_generation: generation,
            attempt_timeout: Duration::from_millis(50),
            retry_delay: Duration::from_millis(1),
            record: Arc::new(move |outcome| recorded.lock().push(outcome.to_owned())),
        };
        (fpr, outcomes, calls)
    }

    async fn settled(outcomes: &Arc<Mutex<Vec<String>>>) -> String {
        for _ in 0..500 {
            if let Some(outcome) = outcomes.lock().first().cloned() {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no outcome recorded");
    }

    fn alive() -> Exited {
        Arc::new(|| false)
    }

    fn constant(generation: u64) -> PauseGeneration {
        Arc::new(move || generation)
    }

    #[tokio::test]
    async fn an_inline_success_is_recorded_as_inline() {
        let (fpr, outcomes, _) = deps(|_| Ok(()), constant(1), alive());

        fpr.run().await;

        assert_eq!(settled(&outcomes).await, "inline");
    }

    #[tokio::test]
    async fn a_transient_failure_succeeds_on_a_retry() {
        let (fpr, outcomes, calls) = deps(
            |call| if call < 3 { Err("transient".to_owned()) } else { Ok(()) },
            constant(1),
            alive(),
        );

        fpr.run().await;

        assert_eq!(settled(&outcomes).await, "retry");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_generation_fence_abandons_a_stale_retry() {
        let generation = Arc::new(AtomicU64::new(1));
        let moved = generation.clone();
        let seen = generation.clone();
        let (fpr, outcomes, calls) = deps(
            move |call| {
                if call == 2 {
                    moved.store(2, Ordering::SeqCst);
                }
                Err("still failing".to_owned())
            },
            Arc::new(move || seen.load(Ordering::SeqCst)),
            alive(),
        );

        fpr.run().await;

        assert_eq!(settled(&outcomes).await, "fenced");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the stale retry must not touch FPR again after the fence moved"
        );
    }

    #[tokio::test]
    async fn a_firecracker_exit_ends_the_retries() {
        let (fpr, outcomes, _) = deps(|_| Err("down".to_owned()), constant(1), Arc::new(|| true));

        fpr.run().await;

        assert_eq!(settled(&outcomes).await, "fc_exited");
    }

    #[tokio::test]
    async fn exhausting_the_retries_is_abandoned() {
        let (fpr, outcomes, calls) = deps(|_| Err("never".to_owned()), constant(1), alive());

        fpr.run().await;

        assert_eq!(settled(&outcomes).await, "abandoned");
        assert_eq!(calls.load(Ordering::SeqCst), 5, "one inline attempt plus four retries");
    }
}
