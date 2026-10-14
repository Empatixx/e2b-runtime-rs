use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

use super::freeze::{WORKLOAD_PROCESS_TYPES, WorkloadFreezer};
use super::hierarchy::ancestor_chain;
use super::{ProcessType, freeze_requested_at, frozen_at};

const MAX_SPAWN_WEIGHT: u32 = 1 << 20;
const SPAWN_ADMIT_WAIT: Duration = Duration::from_millis(100);
pub const SPAWN_DRAIN_WAIT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRefused {
    WorkloadFrozen(Option<String>),
    DrainTimeout,
}

impl fmt::Display for SpawnRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnRefused::WorkloadFrozen(None) => f.write_str("the target cgroup is frozen"),
            SpawnRefused::WorkloadFrozen(Some(detail)) => write!(f, "the target cgroup is frozen: {detail}"),
            SpawnRefused::DrainTimeout => f.write_str("timed out waiting for an in-flight process spawn"),
        }
    }
}

impl std::error::Error for SpawnRefused {}

pub struct SpawnPermit {
    _admission: Option<OwnedSemaphorePermit>,
}

impl SpawnPermit {
    fn unbarriered() -> Self {
        Self { _admission: None }
    }
}

pub struct SpawnBarrier {
    semaphore: Arc<Semaphore>,
    drain_wait: Duration,
    admit_wait: Duration,
    raising: tokio::sync::Mutex<()>,
    held: Mutex<bool>,
}

impl Default for SpawnBarrier {
    fn default() -> Self {
        Self::with_waits(SPAWN_DRAIN_WAIT, SPAWN_ADMIT_WAIT)
    }
}

impl SpawnBarrier {
    pub fn with_waits(drain_wait: Duration, admit_wait: Duration) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(MAX_SPAWN_WEIGHT as usize)),
            drain_wait,
            admit_wait,
            raising: tokio::sync::Mutex::new(()),
            held: Mutex::new(false),
        }
    }

    pub fn is_held(&self) -> bool {
        *self.held.lock()
    }

    #[cfg(test)]
    pub(super) fn drain_is_queued(&self) -> bool {
        self.semaphore.try_acquire().is_err()
    }

    pub async fn raise(&self) -> Result<(), SpawnRefused> {
        let _one_raise_at_a_time = self.raising.lock().await;
        if self.is_held() {
            return Ok(());
        }
        let drained = timeout(
            self.drain_wait,
            self.semaphore.clone().acquire_many_owned(MAX_SPAWN_WEIGHT),
        )
        .await;
        match drained {
            Ok(Ok(everything)) => {
                everything.forget();
                *self.held.lock() = true;
                Ok(())
            }
            _ => Err(SpawnRefused::DrainTimeout),
        }
    }

    pub fn lower(&self) {
        let mut held = self.held.lock();
        if std::mem::take(&mut *held) {
            self.semaphore.add_permits(MAX_SPAWN_WEIGHT as usize);
        }
    }

    pub async fn enter(&self) -> Result<SpawnPermit, SpawnRefused> {
        if let Ok(admission) = self.semaphore.clone().try_acquire_owned() {
            return Ok(SpawnPermit {
                _admission: Some(admission),
            });
        }
        if self.is_held() {
            return Err(SpawnRefused::WorkloadFrozen(None));
        }
        match timeout(self.admit_wait, self.semaphore.clone().acquire_owned()).await {
            Ok(Ok(admission)) => Ok(SpawnPermit {
                _admission: Some(admission),
            }),
            _ => Err(SpawnRefused::WorkloadFrozen(None)),
        }
    }
}

impl WorkloadFreezer {
    pub async fn begin_spawn(&self, process_type: ProcessType) -> Result<SpawnPermit, SpawnRefused> {
        let permit = match WORKLOAD_PROCESS_TYPES.contains(&process_type) {
            true => self.spawns.enter().await?,
            false => SpawnPermit::unbarriered(),
        };
        self.probe_thawed(process_type)?;
        Ok(permit)
    }

    pub fn probe_thawed(&self, process_type: ProcessType) -> Result<(), SpawnRefused> {
        let manager = self.manager();
        let Some(path) = manager.path_of(process_type) else {
            return Ok(());
        };
        if frozen_at(path).is_ok_and(|frozen| frozen) {
            return Err(SpawnRefused::WorkloadFrozen(Some(format!(
                "{} reads frozen",
                path.display()
            ))));
        }
        match ancestor_chain(manager.root(), path)
            .into_iter()
            .find(|ancestor| freeze_requested_at(ancestor).is_ok_and(|requested| requested))
        {
            Some(ancestor) => Err(SpawnRefused::WorkloadFrozen(Some(format!(
                "{} has a freeze requested",
                ancestor.display()
            )))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn queued_drain(
        barrier: &Arc<SpawnBarrier>,
    ) -> (SpawnPermit, tokio::task::JoinHandle<Result<(), SpawnRefused>>) {
        let in_flight = barrier.enter().await.unwrap();
        let raising = tokio::spawn({
            let barrier = barrier.clone();
            async move { barrier.raise().await }
        });
        for _ in 0..1000 {
            if barrier.drain_is_queued() {
                return (in_flight, raising);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("the raise never queued its drain");
    }

    #[tokio::test]
    async fn raise_is_serialized_and_lower_is_idempotent() {
        let barrier = Arc::new(SpawnBarrier::default());

        let raises: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                tokio::spawn(async move { barrier.raise().await })
            })
            .collect();
        for raise in raises {
            assert_eq!(raise.await.unwrap(), Ok(()));
        }

        barrier.lower();
        barrier.lower();

        barrier
            .enter()
            .await
            .expect("the barrier must be fully released after one lower");
    }

    #[tokio::test]
    async fn a_spawn_meeting_a_drain_gives_up_at_its_own_bound() {
        let barrier = Arc::new(SpawnBarrier::with_waits(
            Duration::from_secs(3600),
            Duration::from_millis(20),
        ));
        let (_in_flight, raising) = queued_drain(&barrier).await;

        let refused = barrier.enter().await;

        assert_eq!(refused.err(), Some(SpawnRefused::WorkloadFrozen(None)));
        assert!(!raising.is_finished(), "the spawn waited for the drain to end");
        raising.abort();
    }

    #[tokio::test]
    async fn a_held_barrier_refuses_without_waiting() {
        let barrier = SpawnBarrier::with_waits(Duration::from_millis(250), Duration::from_secs(3600));
        barrier.raise().await.unwrap();

        let refused = tokio::time::timeout(Duration::from_secs(5), barrier.enter())
            .await
            .expect("the spawn queued on the semaphore");

        assert_eq!(refused.err(), Some(SpawnRefused::WorkloadFrozen(None)));
    }

    #[tokio::test]
    async fn a_drain_that_times_out_acquires_nothing() {
        let barrier = SpawnBarrier::with_waits(Duration::from_millis(20), Duration::from_millis(20));
        let in_flight = barrier.enter().await.unwrap();

        assert_eq!(barrier.raise().await, Err(SpawnRefused::DrainTimeout));
        assert!(!barrier.is_held());

        drop(in_flight);
        barrier.raise().await.unwrap();
        assert!(barrier.is_held());
    }

    #[test]
    fn the_refusals_read_like_upstream() {
        assert_eq!(
            SpawnRefused::WorkloadFrozen(None).to_string(),
            "the target cgroup is frozen"
        );
        assert_eq!(
            SpawnRefused::WorkloadFrozen(Some("/sys/fs/cgroup/user reads frozen".to_owned())).to_string(),
            "the target cgroup is frozen: /sys/fs/cgroup/user reads frozen"
        );
        assert_eq!(
            SpawnRefused::DrainTimeout.to_string(),
            "timed out waiting for an in-flight process spawn"
        );
    }
}
