mod semaphore;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::task::JoinHandle;
use tonic::Status;

pub use semaphore::{AdjustableSemaphore, StartPermit};

use crate::featureflags::{FeatureFlags, MAX_SANDBOXES_PER_NODE, MAX_STARTING_INSTANCES_PER_NODE};

pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
pub const LIMIT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartKind {
    Resume,
    Fresh,
}

#[derive(Debug, Clone, Default)]
pub struct MaxSandboxes {
    limit: Arc<AtomicI64>,
}

impl MaxSandboxes {
    pub fn new(limit: i64) -> Self {
        Self {
            limit: Arc::new(AtomicI64::new(limit)),
        }
    }

    pub fn load(&self) -> i64 {
        self.limit.load(Ordering::SeqCst)
    }

    fn store(&self, limit: i64) {
        self.limit.store(limit, Ordering::SeqCst);
    }
}

pub struct Admission {
    max_sandboxes: MaxSandboxes,
    starting: Arc<AdjustableSemaphore>,
    refresh: OnceLock<JoinHandle<()>>,
}

impl Admission {
    pub fn new(max_sandboxes: i64, starting_limit: i64) -> Result<Self, String> {
        AdjustableSemaphore::new(starting_limit)
            .map(|starting| Self {
                max_sandboxes: MaxSandboxes::new(max_sandboxes),
                starting,
                refresh: OnceLock::new(),
            })
            .map_err(|e| format!("failed to create starting sandboxes semaphore: {e}"))
    }

    pub fn from_flags(flags: &FeatureFlags) -> Result<Self, String> {
        Self::new(
            flags.get(&MAX_SANDBOXES_PER_NODE, &[]),
            flags.get(&MAX_STARTING_INSTANCES_PER_NODE, &[]),
        )
    }

    pub fn start_refresh(&self, flags: FeatureFlags, interval: Duration) {
        let max_sandboxes = self.max_sandboxes.clone();
        let starting = self.starting.clone();
        self.refresh.get_or_init(|| {
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
                loop {
                    ticker.tick().await;
                    refresh_limits(&flags, &max_sandboxes, &starting);
                }
            })
        });
    }

    pub fn max_sandboxes(&self) -> MaxSandboxes {
        self.max_sandboxes.clone()
    }

    pub async fn admit(&self, running: usize, kind: StartKind) -> Result<StartPermit, Status> {
        let max_sandboxes = self.max_sandboxes.load();
        if running as i64 >= max_sandboxes {
            return Err(Status::resource_exhausted(format!(
                "max number of running sandboxes on node reached ({max_sandboxes}), please retry"
            )));
        }
        match kind {
            StartKind::Resume => self.acquire().await,
            StartKind::Fresh => self.try_acquire(),
        }
    }

    fn try_acquire(&self) -> Result<StartPermit, Status> {
        self.starting
            .try_acquire()
            .ok_or_else(|| Status::resource_exhausted("too many sandboxes starting on this node, please retry"))
    }

    pub async fn acquire(&self) -> Result<StartPermit, Status> {
        tokio::time::timeout(ACQUIRE_TIMEOUT, self.starting.acquire())
            .await
            .map_err(|_| Status::resource_exhausted("too many sandboxes resuming on this node, please retry"))
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.refresh.get().iter().for_each(|refresh| refresh.abort());
    }
}

fn refresh_limits(flags: &FeatureFlags, max_sandboxes: &MaxSandboxes, starting: &AdjustableSemaphore) {
    apply_limits(
        max_sandboxes,
        starting,
        flags.get(&MAX_SANDBOXES_PER_NODE, &[]),
        flags.get(&MAX_STARTING_INSTANCES_PER_NODE, &[]),
    );
}

fn apply_limits(max_sandboxes: &MaxSandboxes, starting: &AdjustableSemaphore, max: i64, starting_limit: i64) {
    max_sandboxes.store(max);
    if starting_limit > 0
        && let Err(e) = starting.set_limit(starting_limit)
    {
        tracing::error!(limit = starting_limit, error = %e, "failed to adjust starting sandboxes semaphore");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_node_rejects_new_sandboxes() {
        let admission = Admission::new(2, 3).unwrap();

        let error = admission.admit(2, StartKind::Resume).await.unwrap_err();

        assert_eq!(
            (error.code(), error.message()),
            (
                tonic::Code::ResourceExhausted,
                "max number of running sandboxes on node reached (2), please retry"
            )
        );
        assert!(Admission::new(0, 3).unwrap().admit(0, StartKind::Resume).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn starts_wait_15_seconds_for_a_permit() {
        let admission = Admission::new(200, 1).unwrap();
        let held = admission.admit(0, StartKind::Resume).await.unwrap();

        let started = tokio::time::Instant::now();
        let error = admission.admit(0, StartKind::Resume).await.unwrap_err();

        assert_eq!(started.elapsed(), ACQUIRE_TIMEOUT);
        assert_eq!(
            error.message(),
            "too many sandboxes resuming on this node, please retry"
        );
        drop(held);
        assert!(admission.admit(0, StartKind::Resume).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_start_does_not_wait_for_a_permit() {
        let admission = Admission::new(200, 1).unwrap();
        let held = admission.admit(0, StartKind::Fresh).await.unwrap();

        let started = tokio::time::Instant::now();
        let error = admission.admit(0, StartKind::Fresh).await.unwrap_err();

        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(
            (error.code(), error.message()),
            (
                tonic::Code::ResourceExhausted,
                "too many sandboxes starting on this node, please retry"
            )
        );
        drop(held);
        assert!(admission.admit(0, StartKind::Fresh).await.is_ok());
    }

    #[test]
    fn a_refresh_stores_the_max_and_resizes_the_starting_limit_only_when_positive() {
        let max_sandboxes = MaxSandboxes::new(200);
        let starting = AdjustableSemaphore::new(3).unwrap();

        apply_limits(&max_sandboxes, &starting, 50, 5);
        assert_eq!((max_sandboxes.load(), starting.limit()), (50, 5));

        apply_limits(&max_sandboxes, &starting, 0, 0);
        assert_eq!((max_sandboxes.load(), starting.limit()), (0, 5));

        apply_limits(&max_sandboxes, &starting, 7, -1);
        assert_eq!((max_sandboxes.load(), starting.limit()), (7, 5));
    }

    #[test]
    fn the_flag_fallbacks_are_the_initial_limits() {
        let admission = Admission::from_flags(&FeatureFlags::offline()).unwrap();

        assert_eq!(admission.max_sandboxes().load(), 200);
        assert_eq!(admission.starting.limit(), 3);
    }

    #[test]
    fn a_non_positive_starting_limit_fails_like_upstream() {
        assert_eq!(
            Admission::new(200, 0).err(),
            Some(
                "failed to create starting sandboxes semaphore: NewAdjustableSemaphore: limit must be > 0, got: 0"
                    .to_owned()
            )
        );
    }
}
