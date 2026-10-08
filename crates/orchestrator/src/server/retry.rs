use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
pub const TOTAL_BUDGET: Duration = Duration::from_secs(2 * 60 * 60);
const FIRST_DELAY: Duration = Duration::from_secs(5);
const MAX_DELAY: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Retryable(String),
    Terminal(String),
}

pub fn delays() -> impl Iterator<Item = Duration> {
    std::iter::successors(Some(FIRST_DELAY), |delay| Some((*delay * 2).min(MAX_DELAY)))
}

pub async fn retry<F, Fut>(mut attempt: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), Failure>>,
{
    let deadline = Instant::now() + TOTAL_BUDGET;
    let mut delays = delays();
    loop {
        let budget = ATTEMPT_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        let error = match tokio::time::timeout(budget, attempt()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(Failure::Terminal(e))) => return Err(e),
            Ok(Err(Failure::Retryable(e))) => e,
            Err(_) => format!("attempt timed out after {budget:?}"),
        };
        let delay = delays.next().unwrap_or(MAX_DELAY);
        if Instant::now() + delay >= deadline {
            return Err(error);
        }
        tracing::warn!(error = %error, retry_in = ?delay, "upload attempt failed");
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;

    #[test]
    fn delays_double_from_5_seconds_up_to_2_minutes() {
        let seconds: Vec<u64> = delays().take(7).map(|delay| delay.as_secs()).collect();
        assert_eq!(seconds, vec![5, 10, 20, 40, 80, 120, 120]);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_an_attempt_succeeds() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let seen = calls.clone();

        let result = retry(|| {
            let seen = seen.clone();
            async move {
                let mut calls = seen.lock();
                calls.push(Instant::now());
                match calls.len() {
                    3 => Ok(()),
                    _ => Err(Failure::Retryable("s3 unavailable".to_owned())),
                }
            }
        })
        .await;

        assert_eq!(result, Ok(()));
        let offsets: Vec<u64> = calls.lock().iter().map(|at| (*at - started).as_secs()).collect();
        assert_eq!(offsets, vec![0, 5, 15]);
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_failures_stop_at_once_and_the_budget_ends_retries() {
        let terminal = retry(|| async { Err(Failure::Terminal("snapshot files are gone".to_owned())) }).await;
        assert_eq!(terminal, Err("snapshot files are gone".to_owned()));

        let started = Instant::now();
        let exhausted = retry(|| async { Err(Failure::Retryable("s3 unavailable".to_owned())) }).await;
        assert_eq!(exhausted, Err("s3 unavailable".to_owned()));
        assert!(started.elapsed() <= TOTAL_BUDGET);
        assert!(started.elapsed() > TOTAL_BUDGET - MAX_DELAY);
    }
}
