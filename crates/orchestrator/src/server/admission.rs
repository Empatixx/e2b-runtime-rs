use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::Status;

pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Admission {
    max_sandboxes: i64,
    starting: Arc<Semaphore>,
}

impl Admission {
    pub fn new(max_sandboxes: i64, max_starting: usize) -> Self {
        Self {
            max_sandboxes,
            starting: Arc::new(Semaphore::new(max_starting)),
        }
    }

    pub async fn admit(&self, running: usize) -> Result<OwnedSemaphorePermit, Status> {
        if running as i64 >= self.max_sandboxes {
            return Err(Status::resource_exhausted(format!(
                "max number of running sandboxes on node reached ({}), please retry",
                self.max_sandboxes
            )));
        }
        tokio::time::timeout(ACQUIRE_TIMEOUT, self.starting.clone().acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| Status::resource_exhausted("too many sandboxes resuming on this node, please retry"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_node_rejects_new_sandboxes() {
        let admission = Admission::new(2, 3);

        let error = admission.admit(2).await.unwrap_err();

        assert_eq!(
            (error.code(), error.message()),
            (
                tonic::Code::ResourceExhausted,
                "max number of running sandboxes on node reached (2), please retry"
            )
        );
        assert!(Admission::new(0, 3).admit(0).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn starts_wait_15_seconds_for_a_permit() {
        let admission = Admission::new(200, 1);
        let held = admission.admit(0).await.unwrap();

        let started = tokio::time::Instant::now();
        let error = admission.admit(0).await.unwrap_err();

        assert_eq!(started.elapsed(), ACQUIRE_TIMEOUT);
        assert_eq!(
            error.message(),
            "too many sandboxes resuming on this node, please retry"
        );
        drop(held);
        assert!(admission.admit(0).await.is_ok());
    }
}
