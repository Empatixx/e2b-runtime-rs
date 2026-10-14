use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::sync::Mutex;

use crate::checkpoint::new_id;
use crate::firecracker::FirecrackerProcess;
use crate::host::Host;
use crate::network::Slot;

pub struct WarmVm {
    pub dir: PathBuf,
    pub slot: Slot,
    pub process: FirecrackerProcess,
}

pub struct WarmPool {
    host: Arc<Host>,
    ready: Mutex<Vec<WarmVm>>,
    size: usize,
    refilling: AtomicBool,
}

impl WarmVm {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn spawn(host: &Host) -> Result<Self> {
        let dir = host.config.sandboxes_dir().join(new_id("vm-"));
        std::fs::create_dir_all(&dir)?;
        let slot = host.network.acquire().await?;
        match FirecrackerProcess::spawn(&host.config.firecracker, &slot.namespace, &dir).await {
            Ok(process) => Ok(Self { dir, slot, process }),
            Err(e) => {
                host.network.release(slot).await;
                Err(e)
            }
        }
    }
}

impl WarmPool {
    pub fn new(host: Arc<Host>, size: usize) -> Arc<Self> {
        let pool = Arc::new(Self {
            host,
            ready: Mutex::default(),
            size,
            refilling: AtomicBool::new(false),
        });
        pool.refill_in_background();
        pool
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn take(self: &Arc<Self>) -> Result<WarmVm> {
        let warm = self.ready.lock().await.pop();
        self.refill_in_background();
        match warm {
            Some(warm) => Ok(warm),
            None => WarmVm::spawn(&self.host).await,
        }
    }

    fn refill_in_background(self: &Arc<Self>) {
        if self.refilling.swap(true, Ordering::SeqCst) {
            return;
        }
        let pool = self.clone();
        tokio::spawn(async move {
            pool.refill().await;
            pool.refilling.store(false, Ordering::SeqCst);
        });
    }

    async fn refill(&self) {
        while self.ready.lock().await.len() < self.size {
            match WarmVm::spawn(&self.host).await {
                Ok(warm) => self.ready.lock().await.push(warm),
                Err(e) => {
                    tracing::warn!(error = format!("{e:#}"), "warm pool refill failed");
                    return;
                }
            }
        }
    }
}
