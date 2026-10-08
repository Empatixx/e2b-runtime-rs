use std::pin::Pin;
use std::time::Duration;

use tokio::time::{Instant, Sleep, sleep};

pub struct RetryTimer {
    delay: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl RetryTimer {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            sleep: Box::pin(sleep(delay)),
        }
    }

    pub fn arm(&mut self) {
        self.sleep.as_mut().reset(Instant::now() + self.delay);
    }

    pub async fn elapsed(&mut self) {
        self.sleep.as_mut().await;
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_deadline_survives_dropped_waits() {
        let mut timer = RetryTimer::new(Duration::from_millis(10));
        timer.arm();

        let early = [Duration::from_millis(4); 2];
        for slice in early {
            assert!(timeout(slice, timer.elapsed()).await.is_err());
        }

        assert!(timeout(Duration::from_millis(4), timer.elapsed()).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn arming_again_restarts_the_delay() {
        let mut timer = RetryTimer::new(Duration::from_millis(10));
        timer.arm();
        timer.elapsed().await;

        timer.arm();

        assert!(timeout(Duration::from_millis(5), timer.elapsed()).await.is_err());
        assert!(timeout(Duration::from_millis(6), timer.elapsed()).await.is_ok());
    }
}
