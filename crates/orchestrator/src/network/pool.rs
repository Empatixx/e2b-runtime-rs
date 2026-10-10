use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument;

use super::firewall::EgressRules;
use super::slot::Slot;
use super::storage::SlotStorage;
use crate::telemetry::server::network::{SlotReturn, SlotReturnResult};

pub const NEW_SLOTS_POOL_SIZE: usize = 32;
pub const REUSED_SLOTS_POOL_SIZE: usize = 100;
pub const RETURN_DELAY: Duration = Duration::from_secs(3);
fn closed() -> String {
    "network pool is closed".to_owned()
}

const RETRY_DELAY: Duration = Duration::from_millis(50);

pub trait SlotFactory: Send + Sync + 'static {
    fn create(&self, index: u32) -> impl Future<Output = Result<Slot, String>> + Send;
    fn destroy(&self, slot: Slot) -> impl Future<Output = Result<(), String>> + Send;
    fn configure(&self, slot: Slot, rules: &EgressRules) -> impl Future<Output = Result<(), String>> + Send;
    fn deny_egress(&self, slot: Slot) -> impl Future<Output = Result<(), String>> + Send;
}

pub struct SlotLease<F: SlotFactory> {
    pool: Arc<NetworkPool<F>>,
    slot: Option<Slot>,
}

impl<F: SlotFactory> SlotLease<F> {
    pub fn slot(&self) -> Option<Slot> {
        self.slot
    }

    pub fn keep(mut self) -> Option<Slot> {
        self.slot.take()
    }
}

impl<F: SlotFactory> Drop for SlotLease<F> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.pool.return_slot(slot);
        }
    }
}

pub struct NetworkPool<F> {
    factory: F,
    storage: SlotStorage,
    network_version: u8,
    new_slots: tokio::sync::Mutex<mpsc::Receiver<Slot>>,
    new_sender: mpsc::Sender<Slot>,
    reused: Mutex<VecDeque<Slot>>,
    reused_added: Notify,
    custom_rules: Mutex<HashSet<u32>>,
    tasks: TaskTracker,
    done: CancellationToken,
}

impl<F: SlotFactory> NetworkPool<F> {
    pub fn new(factory: F, storage: SlotStorage, network_version: u8) -> Arc<Self> {
        let (new_sender, new_slots) = mpsc::channel(NEW_SLOTS_POOL_SIZE - 1);
        Arc::new(Self {
            factory,
            storage,
            network_version,
            new_slots: tokio::sync::Mutex::new(new_slots),
            new_sender,
            reused: Mutex::default(),
            reused_added: Notify::new(),
            custom_rules: Mutex::default(),
            tasks: TaskTracker::new(),
            done: CancellationToken::new(),
        })
    }

    pub fn start(self: &Arc<Self>) {
        self.tasks.spawn(self.clone().populate());
    }

    pub async fn get(&self) -> Result<Slot, String> {
        loop {
            if let Some(slot) = self.reused.lock().pop_front() {
                return Ok(slot);
            }
            tokio::select! {
                slot = async { self.new_slots.lock().await.recv().await } => return slot.ok_or_else(closed),
                () = self.reused_added.notified() => {}
                () = self.done.cancelled() => return Err(closed()),
            }
        }
    }

    pub async fn lease(self: &Arc<Self>, egress: Option<&EgressRules>) -> Result<SlotLease<F>, String> {
        let slot = self.get().await?;
        let lease = SlotLease {
            pool: self.clone(),
            slot: Some(slot),
        };
        let Some(rules) = egress else {
            return Ok(lease);
        };
        match self.configure(slot, rules).await {
            Ok(()) => Ok(lease),
            Err(e) => {
                self.return_now(lease);
                Err(e)
            }
        }
    }

    fn return_now(self: &Arc<Self>, lease: SlotLease<F>) {
        if let Some(slot) = lease.keep() {
            self.return_after(slot, Duration::ZERO, nothing_to_release());
        }
    }

    pub async fn deny_egress(&self, slot: Slot) -> Result<(), String> {
        self.custom_rules.lock().insert(slot.index);
        self.factory.deny_egress(slot).await
    }

    pub async fn configure(&self, slot: Slot, rules: &EgressRules) -> Result<(), String> {
        self.custom_rules.lock().insert(slot.index);
        self.factory.configure(slot, rules).await
    }

    pub fn return_slot(self: &Arc<Self>, slot: Slot) {
        self.return_after(slot, RETURN_DELAY, nothing_to_release());
    }

    pub fn return_slot_with(self: &Arc<Self>, slot: Slot, release: Release) {
        self.return_after(slot, RETURN_DELAY, release);
    }

    pub fn discard_slot(self: &Arc<Self>, slot: Slot) {
        self.discard_slot_with(slot, nothing_to_release());
    }

    pub fn discard_slot_with(self: &Arc<Self>, slot: Slot, release: Release) {
        let pool = self.clone();
        self.tasks.spawn(async move {
            match release().await {
                Ok(()) => pool.destroy(slot).await,
                Err(e) => retained(slot, &e),
            }
        });
    }

    fn return_after(self: &Arc<Self>, slot: Slot, delay: Duration, release: Release) {
        let pool = self.clone();
        self.tasks.spawn(
            async move { pool.return_timed(slot, delay, release).await }.instrument(tracing::info_span!(
                "clean network-slot",
                network_version = self.network_version
            )),
        );
    }

    async fn return_timed(&self, slot: Slot, delay: Duration, release: Release) {
        let started = Instant::now();
        let shutdown = tokio::select! {
            () = tokio::time::sleep(delay) => false,
            () = self.done.cancelled() => true,
        };
        let cleanup_started = Instant::now();
        let returned = self.release_then_settle(slot, release, shutdown).await;
        SlotReturn {
            network_version: self.network_version,
            result: slot_return_result(shutdown, &returned),
            total: started.elapsed(),
            cleanup: cleanup_started.elapsed(),
        }
        .record();
    }

    pub async fn close(&self) {
        self.done.cancel();
        self.tasks.close();
        self.tasks.wait().await;
        let mut new_slots = self.new_slots.lock().await;
        new_slots.close();
        while let Ok(slot) = new_slots.try_recv() {
            self.destroy(slot).await;
        }
        let reused: Vec<Slot> = self.reused.lock().drain(..).collect();
        for slot in reused {
            self.destroy(slot).await;
        }
    }

    async fn populate(self: Arc<Self>) {
        while !self.done.is_cancelled() {
            match self.next_new_slot().await {
                Ok(slot) => self.offer(slot).await,
                Err(e) => {
                    tracing::warn!(error = %e, "could not create a network slot");
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    async fn next_new_slot(&self) -> Result<Slot, String> {
        let index = self.storage.acquire()?;
        self.factory
            .create(index)
            .await
            .inspect_err(|_| self.storage.release(index))
    }

    async fn offer(&self, slot: Slot) {
        tokio::select! {
            sent = self.new_sender.send(slot) => if let Err(mpsc::error::SendError(unsent)) = sent {
                self.destroy(unsent).await;
            },
            () = self.done.cancelled() => self.destroy(slot).await,
        }
    }

    async fn release_then_settle(&self, slot: Slot, release: Release, shutdown: bool) -> Result<(), ReturnFailure> {
        if let Err(e) = release().await {
            retained(slot, &e);
            return Err(ReturnFailure::Retained);
        }
        match shutdown {
            true => self.cleanup(slot).await,
            false => self.recycle(slot).await,
        }
        .map_err(ReturnFailure::Failed)
    }

    async fn recycle(&self, slot: Slot) -> Result<(), String> {
        if self.custom_rules.lock().remove(&slot.index)
            && let Err(e) = self.factory.configure(slot, &EgressRules::default()).await
        {
            tracing::warn!(slot = slot.index, error = %e, "could not reset the slot firewall");
            return self.cleanup(slot).await.and(Err(e));
        }
        let rejected = {
            let mut reused = self.reused.lock();
            match reused.len() < REUSED_SLOTS_POOL_SIZE {
                true => {
                    reused.push_back(slot);
                    self.reused_added.notify_one();
                    None
                }
                false => Some(slot),
            }
        };
        match rejected {
            Some(slot) => self.cleanup(slot).await,
            None => Ok(()),
        }
    }

    async fn destroy(&self, slot: Slot) {
        drop(self.cleanup(slot).await);
    }

    async fn cleanup(&self, slot: Slot) -> Result<(), String> {
        let destroyed = self.factory.destroy(slot).await;
        if let Err(e) = &destroyed {
            tracing::warn!(slot = slot.index, error = %e, "could not remove a network slot");
        }
        self.storage.release(slot.index);
        destroyed
    }
}

fn slot_return_result(shutdown: bool, returned: &Result<(), ReturnFailure>) -> SlotReturnResult {
    match (returned, shutdown) {
        (Err(ReturnFailure::Retained), _) => SlotReturnResult::Retained,
        (Err(ReturnFailure::Failed(_)), _) => SlotReturnResult::Error,
        (Ok(()), true) => SlotReturnResult::Shutdown,
        (Ok(()), false) => SlotReturnResult::Success,
    }
}

fn retained(slot: Slot, error: &str) {
    tracing::error!(
        slot = slot.index,
        error,
        "network slot retained and unavailable for reuse"
    );
}

pub type Release = Box<dyn FnOnce() -> BoxFuture<'static, Result<(), String>> + Send>;

pub fn nothing_to_release() -> Release {
    Box::new(|| std::future::ready(Ok(())).boxed())
}

#[derive(Debug, PartialEq, Eq)]
enum ReturnFailure {
    Retained,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[derive(Default)]
    struct FakeSlots {
        created: Mutex<Vec<u32>>,
        destroyed: Mutex<BTreeSet<u32>>,
        failures: Mutex<u32>,
        configured: Mutex<Vec<(u32, EgressRules)>>,
        configure_failures: Mutex<u32>,
        denied: Mutex<Vec<u32>>,
    }

    impl SlotFactory for Arc<FakeSlots> {
        async fn create(&self, index: u32) -> Result<Slot, String> {
            let mut failures = self.failures.lock();
            if *failures > 0 {
                *failures -= 1;
                return Err("ip: device busy".to_owned());
            }
            self.created.lock().push(index);
            Slot::new(index)
        }

        async fn destroy(&self, slot: Slot) -> Result<(), String> {
            self.destroyed.lock().insert(slot.index);
            Ok(())
        }

        async fn deny_egress(&self, slot: Slot) -> Result<(), String> {
            self.denied.lock().push(slot.index);
            Ok(())
        }

        async fn configure(&self, slot: Slot, rules: &EgressRules) -> Result<(), String> {
            self.configured.lock().push((slot.index, rules.clone()));
            let mut failures = self.configure_failures.lock();
            match *failures {
                0 => Ok(()),
                _ => {
                    *failures -= 1;
                    Err("nft: rule rejected".to_owned())
                }
            }
        }
    }

    fn pool(indices: std::ops::RangeInclusive<u32>) -> (Arc<NetworkPool<Arc<FakeSlots>>>, Arc<FakeSlots>) {
        let slots = Arc::new(FakeSlots::default());
        let pool = NetworkPool::new(slots.clone(), SlotStorage::new(indices, Box::new(|_| false)), 1);
        (pool, slots)
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn populate_keeps_32_new_slots_ready() {
        let (pool, slots) = pool(1..=100);
        pool.start();
        settle().await;

        assert_eq!(slots.created.lock().len(), NEW_SLOTS_POOL_SIZE);
        assert_eq!(pool.get().await.unwrap().index, 1);
        settle().await;
        assert_eq!(slots.created.lock().len(), NEW_SLOTS_POOL_SIZE + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn get_prefers_an_initially_reused_slot() {
        let (pool, _) = pool(1..=100);
        pool.start();
        settle().await;
        let reused = pool.get().await.unwrap();
        pool.reused.lock().push_back(reused);

        assert_eq!(pool.get().await.unwrap(), reused);
        assert!(pool.reused.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn get_makes_progress_with_only_a_new_slot() {
        let (pool, _) = pool(1..=100);
        pool.start();

        assert_eq!(pool.get().await.unwrap().index, 1);
        assert!(pool.reused.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn get_returns_closed_when_the_new_slots_are_closed() {
        let (pool, _) = pool(1..=100);
        pool.new_slots.lock().await.close();

        assert_eq!(pool.get().await, Err("network pool is closed".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn close_unblocks_a_get_waiting_for_a_slot() {
        let (pool, _) = pool(1..=100);
        let waiting = tokio::spawn({
            let pool = pool.clone();
            async move { pool.get().await }
        });
        settle().await;

        pool.close().await;

        assert_eq!(waiting.await.unwrap(), Err("network pool is closed".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_get_receives_a_slot_returned_in_the_meantime() {
        let (pool, _) = pool(1..=100);
        let waiting = tokio::spawn({
            let pool = pool.clone();
            async move { pool.get().await }
        });
        settle().await;
        let returned = Slot::new(42).unwrap();

        pool.return_slot(returned);

        assert_eq!(waiting.await.unwrap(), Ok(returned));
    }

    fn failing_release() -> Release {
        Box::new(|| std::future::ready(Err("boom".to_owned())).boxed())
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_release_keeps_the_slot_allocated_and_out_of_reuse_on_every_path() {
        let (pool, slots) = pool(1..=100);
        pool.start();
        let normal = pool.get().await.unwrap();
        let shutdown = pool.get().await.unwrap();
        pool.return_slot_with(normal, failing_release());
        tokio::time::sleep(RETURN_DELAY + Duration::from_millis(10)).await;
        assert!(pool.reused.lock().is_empty());
        assert!(!slots.destroyed.lock().contains(&normal.index));

        pool.discard_slot_with(shutdown, failing_release());
        pool.close().await;
        pool.return_slot_with(Slot::new(99).unwrap(), failing_release());
        settle().await;

        assert!(!slots.destroyed.lock().contains(&shutdown.index));
        assert!(!slots.destroyed.lock().contains(&99));
        assert!(pool.reused.lock().is_empty());
    }

    #[test]
    fn a_retained_slot_reads_as_retained_in_the_metric() {
        assert_eq!(
            slot_return_result(false, &Err(ReturnFailure::Retained)),
            SlotReturnResult::Retained
        );
        assert_eq!(
            slot_return_result(true, &Err(ReturnFailure::Failed("x".to_owned()))),
            SlotReturnResult::Error
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_returned_slot_is_reused_after_the_return_delay() {
        let (pool, _) = pool(1..=100);
        pool.start();
        let first = pool.get().await.unwrap();
        pool.return_slot(first);

        tokio::time::sleep(RETURN_DELAY - Duration::from_millis(10)).await;
        assert_ne!(pool.get().await.unwrap(), first);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.get().await.unwrap(), first);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_reused_queue_destroys_the_returned_slot() {
        let (pool, slots) = pool(1..=200);
        pool.start();
        let mut taken = Vec::new();
        for _ in 0..=REUSED_SLOTS_POOL_SIZE {
            taken.push(pool.get().await.unwrap());
        }
        let last = *taken.last().unwrap();
        taken.into_iter().for_each(|slot| pool.return_slot(slot));
        tokio::time::sleep(RETURN_DELAY * 2).await;

        assert_eq!(
            slots.destroyed.lock().iter().copied().collect::<Vec<_>>(),
            vec![last.index]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn creation_errors_do_not_stop_populate() {
        let (pool, slots) = pool(1..=100);
        *slots.failures.lock() = 3;
        pool.start();

        assert_eq!(pool.get().await.unwrap().index, 1);
        assert_eq!(slots.created.lock().first(), Some(&1));
    }

    #[tokio::test(start_paused = true)]
    async fn close_waits_for_returns_and_destroys_every_slot() {
        let (pool, slots) = pool(1..=100);
        pool.start();
        let returned = pool.get().await.unwrap();
        settle().await;
        pool.return_slot(returned);

        pool.close().await;

        let created: BTreeSet<u32> = slots.created.lock().iter().copied().collect();
        assert_eq!(*slots.destroyed.lock(), created);
        assert_eq!(pool.get().await, Err("network pool is closed".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_discarded_slot_is_destroyed_instead_of_reused() {
        let (pool, slots) = pool(1..=100);
        pool.start();
        let slot = pool.get().await.unwrap();

        pool.discard_slot(slot);
        tokio::time::sleep(RETURN_DELAY + Duration::from_millis(10)).await;

        assert!(slots.destroyed.lock().contains(&slot.index));
        assert_ne!(pool.get().await.unwrap(), slot);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slot_returned_after_close_is_destroyed_without_the_return_delay() {
        let (pool, slots) = pool(1..=100);
        pool.start();
        let slot = pool.get().await.unwrap();
        pool.close().await;

        pool.return_slot(slot);
        settle().await;

        assert!(slots.destroyed.lock().contains(&slot.index));
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_lease_returns_its_slot_and_a_kept_one_does_not() {
        let (pool, _) = pool(1..=100);
        pool.start();
        let dropped = pool.lease(None).await.unwrap();
        let first = dropped.slot().unwrap();
        drop(dropped);
        let kept = pool.lease(None).await.unwrap().keep().unwrap();

        tokio::time::sleep(RETURN_DELAY + Duration::from_millis(10)).await;

        assert_eq!(pool.get().await.unwrap(), first);
        assert_ne!(kept, first);
    }

    #[tokio::test(start_paused = true)]
    async fn custom_egress_is_applied_on_lease_and_reset_before_reuse() {
        let (pool, slots) = pool(1..=100);
        pool.start();
        let rules = EgressRules {
            byop: false,
            allowed: vec!["8.8.8.8/32".to_owned()],
            denied: vec!["0.0.0.0/0".to_owned()],
        };

        let slot = pool.lease(Some(&rules)).await.unwrap().keep().unwrap();
        pool.return_slot(slot);
        tokio::time::sleep(RETURN_DELAY + Duration::from_millis(10)).await;

        assert_eq!(
            slots.configured.lock().clone(),
            vec![(slot.index, rules), (slot.index, EgressRules::default())]
        );
        assert_eq!(pool.get().await.unwrap(), slot);
    }

    #[tokio::test(start_paused = true)]
    async fn a_denied_slot_is_reset_before_reuse() {
        let (pool, slots) = pool(1..=100);
        pool.start();

        let slot = pool.lease(None).await.unwrap().keep().unwrap();
        pool.deny_egress(slot).await.unwrap();
        pool.return_slot(slot);
        tokio::time::sleep(RETURN_DELAY + Duration::from_millis(10)).await;

        assert_eq!(slots.denied.lock().clone(), vec![slot.index]);
        assert_eq!(
            slots.configured.lock().clone(),
            vec![(slot.index, EgressRules::default())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slot_whose_egress_failed_goes_back_without_the_return_delay() {
        let (pool, slots) = pool(1..=100);
        pool.start();
        let rules = EgressRules {
            byop: false,
            allowed: Vec::new(),
            denied: vec!["0.0.0.0/0".to_owned()],
        };
        *slots.configure_failures.lock() = 1;

        let failed = pool.lease(Some(&rules)).await.map(|lease| lease.slot());
        settle().await;

        assert_eq!(failed, Err("nft: rule rejected".to_owned()));
        let slot = slots.configured.lock()[0].0;
        assert_eq!(
            slots.configured.lock().clone(),
            vec![(slot, rules), (slot, EgressRules::default())]
        );
        assert_eq!(pool.get().await.unwrap().index, slot);
    }
}
