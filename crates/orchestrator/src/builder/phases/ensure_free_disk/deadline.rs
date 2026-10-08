use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

pub const DEADLINE_EXCEEDED: &str = "context deadline exceeded";

#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    pub fn after(duration: Duration) -> Self {
        Self {
            at: Instant::now() + duration,
        }
    }

    pub async fn bound<T>(self, work: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        tokio::time::timeout_at(self.at, work)
            .await
            .unwrap_or_else(|_| Err(DEADLINE_EXCEEDED.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn work_past_the_deadline_fails_like_a_go_context() {
        let deadline = Deadline::after(Duration::from_secs(5));

        let bounded = deadline
            .bound(async {
                tokio::time::sleep(Duration::from_secs(6)).await;
                Ok(1)
            })
            .await;

        assert_eq!(bounded, Err("context deadline exceeded".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn work_within_the_deadline_keeps_its_result() {
        let deadline = Deadline::after(Duration::from_secs(5));

        assert_eq!(deadline.bound(async { Ok(1) }).await, Ok(1));
    }
}
