use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use std::thread::JoinHandle;

use tokio::sync::mpsc;

pub const DEFAULT_POOL_SIZE: usize = 64;
const POPULATE_RETRY: Duration = Duration::from_millis(50);
const RELEASE_RETRY: Duration = Duration::from_millis(500);

pub type FreeCheck = Box<dyn Fn(u32) -> bool + Send + Sync>;

const CLOSED: &str = "nbd device pool is closed";

pub struct DevicePool {
    devices: Arc<Devices>,
    ready: tokio::sync::Mutex<mpsc::Receiver<u32>>,
    populating: Mutex<Option<JoinHandle<()>>>,
}

struct Devices {
    count: u32,
    is_free: FreeCheck,
    claims: Mutex<Claims>,
}

#[derive(Default)]
struct Claims {
    taken: BTreeSet<u32>,
    next: u32,
    closed: bool,
}

impl DevicePool {
    pub fn new(count: u32, ready: usize, is_free: FreeCheck) -> Self {
        let (sender, receiver) = mpsc::channel(ready.clamp(1, count.max(1) as usize));
        let devices = Arc::new(Devices {
            count,
            is_free,
            claims: Mutex::default(),
        });
        let populating = devices.clone();
        let populate = std::thread::spawn(move || populating.populate(&sender));
        Self {
            devices,
            ready: tokio::sync::Mutex::new(receiver),
            populating: Mutex::new(Some(populate)),
        }
    }

    pub async fn get(&self) -> Result<u32, String> {
        let index = self.ready.lock().await.recv().await.ok_or_else(|| CLOSED.to_owned())?;
        self.devices.claim_received(index)
    }

    pub async fn close(&self) -> Vec<u32> {
        self.devices.claims.lock().closed = true;
        self.ready.lock().await.close();
        self.populating.lock().take().into_iter().for_each(|populate| {
            drop(populate.join());
        });
        self.devices.claims.lock().taken.iter().copied().collect()
    }

    pub fn release(&self, index: u32) {
        let devices = self.devices.clone();
        std::thread::spawn(move || devices.release_when_free(index));
    }
}

impl Devices {
    fn populate(&self, sender: &mpsc::Sender<u32>) {
        loop {
            match self.claim_next() {
                Some(index) => {
                    if sender.blocking_send(index).is_err() {
                        self.unclaim(index);
                        return;
                    }
                }
                None if sender.is_closed() => return,
                None => std::thread::sleep(POPULATE_RETRY),
            }
        }
    }

    fn claim_received(&self, index: u32) -> Result<u32, String> {
        let mut claims = self.claims.lock();
        match claims.closed {
            true => {
                claims.taken.remove(&index);
                Err(CLOSED.to_owned())
            }
            false => Ok(index),
        }
    }

    fn claim_next(&self) -> Option<u32> {
        let mut claims = self.claims.lock();
        if claims.closed {
            return None;
        }
        let start = claims.next;
        let index = (0..self.count)
            .map(|step| (start + step) % self.count)
            .find(|index| !claims.taken.contains(index) && (self.is_free)(*index))?;
        claims.taken.insert(index);
        claims.next = index + 1;
        Some(index)
    }

    fn unclaim(&self, index: u32) {
        self.claims.lock().taken.remove(&index);
    }

    fn release_when_free(&self, index: u32) {
        while !(self.is_free)(index) {
            std::thread::sleep(RELEASE_RETRY);
        }
        self.unclaim(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(count: u32, busy: &[u32]) -> (DevicePool, Arc<Mutex<BTreeSet<u32>>>) {
        let busy = Arc::new(Mutex::new(busy.iter().copied().collect::<BTreeSet<u32>>()));
        let check = busy.clone();
        (
            DevicePool::new(count, 2, Box::new(move |index| !check.lock().contains(&index))),
            busy,
        )
    }

    #[tokio::test]
    async fn devices_in_use_are_skipped_and_none_is_handed_out_twice() {
        let (pool, _) = pool(4, &[1]);

        let handed: Vec<u32> = vec![
            pool.get().await.unwrap(),
            pool.get().await.unwrap(),
            pool.get().await.unwrap(),
        ];

        assert_eq!(handed, vec![0, 2, 3]);
    }

    #[tokio::test]
    async fn a_device_received_after_close_is_refused_and_unclaimed() {
        let (pool, _) = pool(4, &[]);
        let ready = pool.ready.lock().await.recv().await.unwrap();
        pool.devices.claims.lock().closed = true;

        assert_eq!(pool.devices.claim_received(ready), Err(CLOSED.to_owned()));
        assert!(!pool.devices.claims.lock().taken.contains(&ready));
    }

    #[tokio::test]
    async fn close_joins_the_populate_thread_and_nothing_is_claimed_afterwards() {
        let (pool, _) = pool(8, &[]);
        let handed = pool.get().await.unwrap();

        let still_taken = pool.close().await;

        assert!(pool.populating.lock().is_none());
        assert!(still_taken.contains(&handed));
        assert_eq!(pool.devices.claim_next(), None);
        assert_eq!(pool.get().await, Err(CLOSED.to_owned()));
        assert_eq!(
            pool.devices.claims.lock().taken.iter().copied().collect::<Vec<_>>(),
            vec![handed]
        );
    }

    #[tokio::test]
    async fn a_released_device_comes_back_only_once_it_is_free() {
        let (pool, busy) = pool(1, &[]);
        let index = pool.get().await.unwrap();
        busy.lock().insert(index);

        pool.release(index);
        let again = tokio::time::timeout(Duration::from_millis(200), pool.get()).await;
        assert!(again.is_err(), "a busy device was handed out");

        busy.lock().remove(&index);
        let again = tokio::time::timeout(Duration::from_secs(2), pool.get()).await;
        assert_eq!(again.unwrap(), Ok(index));
    }
}
