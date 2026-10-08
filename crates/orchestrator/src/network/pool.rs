use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::firewall::EgressRules;
use super::slot::Slot;
use super::storage::SlotStorage;

pub const NEW_SLOTS_POOL_SIZE: usize = 32;
pub const REUSED_SLOTS_POOL_SIZE: usize = 100;
pub const RETURN_DELAY: Duration = Duration::from_secs(3);
const RETRY_DELAY: Duration = Duration::from_millis(50);

pub trait SlotFactory: Send + Sync + 'static {
    fn create(&self, index: u32) -> impl Future<Output = Result<Slot, String>> + Send;
    fn destroy(&self, slot: Slot) -> impl Future<Output = Result<(), String>> + Send;
    fn configure(&self, slot: Slot, rules: &EgressRules) -> impl Future<Output = Result<(), String>> + Send;
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
    new_slots: tokio::sync::Mutex<mpsc::Receiver<Slot>>,
    new_sender: mpsc::Sender<Slot>,
    reused: Mutex<VecDeque<Slot>>,
    custom_rules: Mutex<HashSet<u32>>,
    tasks: TaskTracker,
    done: CancellationToken,
}

impl<F: SlotFactory> NetworkPool<F> {
    pub fn new(factory: F, storage: SlotStorage) -> Arc<Self> {
        let (new_sender, new_slots) = mpsc::channel(NEW_SLOTS_POOL_SIZE - 1);
        Arc::new(Self {
            factory,
            storage,
            new_slots: tokio::sync::Mutex::new(new_slots),
            new_sender,
            reused: Mutex::default(),
            custom_rules: Mutex::default(),
            tasks: TaskTracker::new(),
            done: CancellationToken::new(),
        })
    }

    pub fn start(self: &Arc<Self>) {
        self.tasks.spawn(self.clone().populate());
    }

    pub async fn get(&self) -> Result<Slot, String> {
        if let Some(slot) = self.reused.lock().pop_front() {
            return Ok(slot);
        }
        tokio::select! {
            slot = async { self.new_slots.lock().await.recv().await } => slot.ok_or_else(|| "network pool is closed".to_owned()),
            () = self.done.cancelled() => Err("network pool is closed".to_owned()),
        }
    }

    pub async fn lease(self: &Arc<Self>, egress: Option<&EgressRules>) -> Result<SlotLease<F>, String> {
        let slot = self.get().await?;
        let lease = SlotLease {
            pool: self.clone(),
            slot: Some(slot),
        };
        if let Some(rules) = egress {
            self.configure(slot, rules).await?;
        }
        Ok(lease)
    }

    pub async fn configure(&self, slot: Slot, rules: &EgressRules) -> Result<(), String> {
        self.custom_rules.lock().insert(slot.index);
        self.factory.configure(slot, rules).await
    }

    pub fn return_slot(self: &Arc<Self>, slot: Slot) {
        let pool = self.clone();
        self.tasks.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(RETURN_DELAY) => pool.recycle(slot).await,
                () = pool.done.cancelled() => pool.cleanup(slot).await,
            }
        });
    }

    pub async fn close(&self) {
        self.done.cancel();
        self.tasks.close();
        self.tasks.wait().await;
        let mut new_slots = self.new_slots.lock().await;
        new_slots.close();
        while let Ok(slot) = new_slots.try_recv() {
            self.cleanup(slot).await;
        }
        let reused: Vec<Slot> = self.reused.lock().drain(..).collect();
        for slot in reused {
            self.cleanup(slot).await;
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
                self.cleanup(unsent).await;
            },
            () = self.done.cancelled() => self.cleanup(slot).await,
        }
    }

    async fn recycle(&self, slot: Slot) {
        if self.custom_rules.lock().remove(&slot.index)
            && let Err(e) = self.factory.configure(slot, &EgressRules::default()).await
        {
            tracing::warn!(slot = slot.index, error = %e, "could not reset the slot firewall");
            self.cleanup(slot).await;
            return;
        }
        let rejected = {
            let mut reused = self.reused.lock();
            match reused.len() < REUSED_SLOTS_POOL_SIZE {
                true => {
                    reused.push_back(slot);
                    None
                }
                false => Some(slot),
            }
        };
        if let Some(slot) = rejected {
            self.cleanup(slot).await;
        }
    }

    async fn cleanup(&self, slot: Slot) {
        if let Err(e) = self.factory.destroy(slot).await {
            tracing::warn!(slot = slot.index, error = %e, "could not remove a network slot");
        }
        self.storage.release(slot.index);
    }
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

        async fn configure(&self, slot: Slot, rules: &EgressRules) -> Result<(), String> {
            self.configured.lock().push((slot.index, rules.clone()));
            Ok(())
        }
    }

    fn pool(indices: std::ops::RangeInclusive<u32>) -> (Arc<NetworkPool<Arc<FakeSlots>>>, Arc<FakeSlots>) {
        let slots = Arc::new(FakeSlots::default());
        let pool = NetworkPool::new(slots.clone(), SlotStorage::new(indices, Box::new(|_| false)));
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
}
