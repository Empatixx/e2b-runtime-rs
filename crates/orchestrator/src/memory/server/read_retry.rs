use std::future::Future;
use std::time::Duration;

use backon::{BackoffBuilder, ExponentialBackoff, ExponentialBuilder};

const MAX_RETRIES: usize = 3;
const FIRST_PAUSE: Duration = Duration::from_millis(50);
const MAX_PAUSE: Duration = Duration::from_millis(500);

pub async fn with_read_retries<T, F, Fut>(mut read: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut pauses = pauses();
    let mut attempts = 1;
    loop {
        match (read().await, pauses.next()) {
            (Ok(value), _) => return Ok(value),
            (Err(e), Some(pause)) => {
                tracing::warn!(attempt = attempts, max_attempts = MAX_RETRIES + 1, error = %e, "UFFD serve read error, retrying");
                tokio::time::sleep(pause).await;
                attempts += 1;
            }
            (Err(e), None) => {
                tracing::error!(attempts, error = %e, "UFFD serve data fetch error after retries");
                return Err(format!("failed to read from source after {attempts} attempts: {e}"));
            }
        }
    }
}

fn pauses() -> ExponentialBackoff {
    ExponentialBuilder::default()
        .with_min_delay(FIRST_PAUSE)
        .with_max_delay(MAX_PAUSE)
        .with_max_times(MAX_RETRIES)
        .with_jitter()
        .build()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_read_that_recovers_is_retried_until_it_succeeds() {
        let attempts = Cell::new(0);

        let read = with_read_retries(|| {
            attempts.set(attempts.get() + 1);
            std::future::ready(match attempts.get() {
                3 => Ok("page"),
                _ => Err("not yet".to_owned()),
            })
        })
        .await;

        assert_eq!((read, attempts.get()), (Ok("page"), 3));
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_that_keeps_failing_gives_up_after_four_attempts() {
        let attempts = Cell::new(0);

        let read: Result<(), String> = with_read_retries(|| {
            attempts.set(attempts.get() + 1);
            std::future::ready(Err("boom".to_owned()))
        })
        .await;

        assert_eq!(
            (read, attempts.get()),
            (Err("failed to read from source after 4 attempts: boom".to_owned()), 4)
        );
    }

    #[test]
    fn pauses_double_from_50_ms_three_times_with_jitter_below_the_next_step() {
        let pauses: Vec<Duration> = pauses().collect();

        assert_eq!(pauses.len(), MAX_RETRIES);
        [50, 100, 200].into_iter().zip(&pauses).for_each(|(millis, pause)| {
            let base = Duration::from_millis(millis);
            assert!(*pause >= base && *pause < base * 2, "{pause:?} for {base:?}");
        });
    }
}
