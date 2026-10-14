use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use super::{Disk, Storage};

const KEEP_WARM: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct SharedMemory {
    storage: Arc<Storage>,
    root: PathBuf,
    devices: Arc<Mutex<HashMap<String, SharedDevice>>>,
}

struct SharedDevice {
    disk: Disk,
    _page_cache_holder: std::fs::File,
    users: usize,
    generation: u64,
}

impl SharedMemory {
    pub fn new(storage: Arc<Storage>, root: PathBuf) -> Self {
        Self {
            storage,
            root,
            devices: Arc::default(),
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn acquire(&self, checkpoint_id: &str, layers: &[PathBuf]) -> Result<Disk> {
        let mut devices = self.devices.lock().await;
        if let Some(shared) = devices.get_mut(checkpoint_id) {
            shared.users += 1;
            return Ok(shared.disk.clone());
        }
        let disk = self
            .storage
            .open_read_only(&self.root.join(checkpoint_id), layers)
            .await?;
        let shared = SharedDevice {
            _page_cache_holder: std::fs::File::open(&disk.device)?,
            disk: disk.clone(),
            users: 1,
            generation: 0,
        };
        devices.insert(checkpoint_id.to_owned(), shared);
        Ok(disk)
    }

    pub async fn release(&self, checkpoint_id: &str) {
        let mut devices = self.devices.lock().await;
        let Some(shared) = devices.get_mut(checkpoint_id) else {
            return;
        };
        shared.users -= 1;
        if shared.users == 0 {
            shared.generation += 1;
            self.close_later(checkpoint_id.to_owned(), shared.generation);
        }
    }

    fn close_later(&self, checkpoint_id: String, generation: u64) {
        let shared = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KEEP_WARM).await;
            shared.close_if_unused(&checkpoint_id, generation).await;
        });
    }

    async fn close_if_unused(&self, checkpoint_id: &str, generation: u64) {
        let mut devices = self.devices.lock().await;
        let unused = devices
            .get(checkpoint_id)
            .is_some_and(|shared| shared.users == 0 && shared.generation == generation);
        if let Some(shared) = unused.then(|| devices.remove(checkpoint_id)).flatten() {
            self.storage.close(&shared.disk).await;
        }
    }
}
