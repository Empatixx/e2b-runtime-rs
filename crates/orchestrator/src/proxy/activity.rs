use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::watch;

pub struct Activity {
    in_flight: AtomicUsize,
    idle_since: watch::Sender<Option<Instant>>,
}

pub struct Engaged {
    activity: Arc<Activity>,
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            in_flight: AtomicUsize::new(0),
            idle_since: watch::channel(None).0,
        })
    }

    pub fn engage(self: &Arc<Self>) -> Engaged {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.idle_since.send_replace(None);
        Engaged { activity: self.clone() }
    }

    pub fn start_idle(&self) {
        if self.in_flight.load(Ordering::SeqCst) == 0 {
            self.idle_since
                .send_if_modified(|since| since.replace(Instant::now()).is_none());
        }
    }

    pub async fn idle_for(&self, timeout: Duration) {
        let mut idle_since = self.idle_since.subscribe();
        loop {
            let since = *idle_since.borrow_and_update();
            match since {
                Some(since) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(tokio::time::Instant::from_std(since + timeout)) => return,
                        changed = idle_since.changed() => if changed.is_err() { return },
                    }
                }
                None => {
                    if idle_since.changed().await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

impl Drop for Engaged {
    fn drop(&mut self) {
        if self.activity.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.activity.idle_since.send_replace(Some(Instant::now()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn never_idle_before_the_first_request_finishes() {
        let activity = Activity::new();
        let waited = tokio::time::timeout(Duration::from_secs(3600), activity.idle_for(Duration::from_secs(1))).await;
        assert!(waited.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_after_the_last_request_finishes() {
        let activity = Activity::new();
        drop(activity.engage());
        let waited = tokio::time::timeout(Duration::from_secs(2), activity.idle_for(Duration::from_secs(1))).await;
        assert!(waited.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_in_flight_postpones_the_idle_deadline() {
        let activity = Activity::new();
        drop(activity.engage());
        let busy = activity.engage();
        let waited = tokio::time::timeout(Duration::from_secs(10), activity.idle_for(Duration::from_secs(1))).await;
        assert!(waited.is_err());
        drop(busy);
        let waited = tokio::time::timeout(Duration::from_secs(2), activity.idle_for(Duration::from_secs(1))).await;
        assert!(waited.is_ok());
    }
}
