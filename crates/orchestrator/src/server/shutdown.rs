use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

pub const DRAIN_POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    Empty,
    Forced,
}

pub async fn drain(remaining: impl Fn() -> usize, forced: impl Future<Output = ()>) -> Drained {
    tracing::info!(live_sandboxes = remaining(), "starting graceful sandbox drain");
    let started = Instant::now();
    let emptied = async {
        let mut polls = tokio::time::interval(DRAIN_POLL);
        let mut last_logged = started;
        while remaining() > 0 {
            polls.tick().await;
            let now = Instant::now();
            if remaining() > 0 && now - last_logged >= log_interval(now - started) {
                tracing::info!(
                    remaining_sandboxes = remaining(),
                    elapsed = ?(now - started),
                    "waiting for sandbox drain"
                );
                last_logged = now;
            }
        }
        tracing::info!(live_sandboxes = 0, "graceful sandbox drain complete");
    };
    tokio::select! {
        () = emptied => Drained::Empty,
        () = forced => {
            tracing::warn!(remaining_sandboxes = remaining(), "graceful sandbox drain timed out");
            Drained::Forced
        }
    }
}

fn log_interval(elapsed: Duration) -> Duration {
    match elapsed {
        elapsed if elapsed < Duration::from_secs(60) => Duration::from_secs(5),
        elapsed if elapsed < Duration::from_secs(3600) => Duration::from_secs(60),
        _ => Duration::from_secs(15 * 60),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn drain_waits_until_the_node_is_empty() {
        let empty = Arc::new(AtomicBool::new(false));
        let emptied = empty.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(12)).await;
            emptied.store(true, Ordering::SeqCst);
        });
        let started = tokio::time::Instant::now();

        let drained = drain(|| usize::from(!empty.load(Ordering::SeqCst)), std::future::pending()).await;

        assert_eq!(drained, Drained::Empty);
        assert_eq!(started.elapsed(), Duration::from_secs(15));
    }

    #[test]
    fn progress_is_logged_less_often_the_longer_the_drain_runs() {
        assert_eq!(log_interval(Duration::from_secs(10)), Duration::from_secs(5));
        assert_eq!(log_interval(Duration::from_secs(600)), Duration::from_secs(60));
        assert_eq!(log_interval(Duration::from_secs(7200)), Duration::from_secs(900));
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_signal_forces_the_stop() {
        let drained = drain(|| 1, tokio::time::sleep(Duration::from_secs(1))).await;

        assert_eq!(drained, Drained::Forced);
    }
}
