use std::future::Future;
use std::time::Duration;

pub const DRAIN_POLL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    Empty,
    Forced,
}

pub async fn drain(is_empty: impl Fn() -> bool, forced: impl Future<Output = ()>) -> Drained {
    let emptied = async {
        let mut polls = tokio::time::interval(DRAIN_POLL);
        while !is_empty() {
            polls.tick().await;
        }
    };
    tokio::select! {
        () = emptied => Drained::Empty,
        () = forced => Drained::Forced,
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

        let drained = drain(|| empty.load(Ordering::SeqCst), std::future::pending()).await;

        assert_eq!(drained, Drained::Empty);
        assert_eq!(started.elapsed(), Duration::from_secs(15));
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_signal_forces_the_stop() {
        let drained = drain(|| false, tokio::time::sleep(Duration::from_secs(1))).await;

        assert_eq!(drained, Drained::Forced);
    }
}
