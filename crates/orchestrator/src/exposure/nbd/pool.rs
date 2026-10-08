use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

pub const DEFAULT_POOL_SIZE: usize = 64;
const POPULATE_RETRY: Duration = Duration::from_millis(50);
const RELEASE_RETRY: Duration = Duration::from_millis(500);

pub type FreeCheck = Box<dyn Fn(u32) -> bool + Send + Sync>;

pub struct DevicePool {
    devices: Arc<Devices>,
    ready: tokio::sync::Mutex<mpsc::Receiver<u32>>,
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
        std::thread::spawn(move || populating.populate(&sender));
        Self {
            devices,
            ready: tokio::sync::Mutex::new(receiver),
        }
    }

    pub async fn get(&self) -> Result<u32, String> {
        self.ready
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| "nbd device pool is closed".to_owned())
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

    fn claim_next(&self) -> Option<u32> {
        let mut claims = self.claims.lock();
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
