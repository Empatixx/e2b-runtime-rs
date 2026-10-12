pub mod device;
pub mod queue;
mod wakeup;

use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};

use crate::block::overlay::Overlay;
use crate::exposure::block_device;

const NOT_STARTED: &str = "ublk device stopped before starting";

type DeviceThread = JoinHandle<Result<(), String>>;

pub struct Exposure {
    id: u32,
    thread: Option<DeviceThread>,
    serving: watch::Receiver<()>,
}

pub async fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
    let (started, id) = oneshot::channel();
    let (alive, serving) = watch::channel(());
    let (size, runtime) = (overlay.size(), Handle::current());
    let thread = std::thread::Builder::new().name("ublk".to_owned()).spawn(move || {
        let served = device::run(overlay, size, runtime, started);
        drop(alive);
        served
    })?;
    let id = match id.await {
        Ok(id) => id,
        Err(_) => return Err(io::Error::other(failure(thread).await)),
    };
    let exposure = Exposure {
        id,
        thread: Some(thread),
        serving,
    };
    device::wait_for_node(&exposure.path()).await?;
    Ok(exposure)
}

impl Exposure {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/dev/ublkb{}", self.id))
    }

    pub async fn flush(&self) -> Result<(), String> {
        let path = self.path();
        tokio::task::spawn_blocking(move || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| format!("sync ublk device: {e}"))
                .and_then(|device| block_device::flush(&device, "ublk"))
        })
        .await
        .map_err(|e| format!("flush ublk device: {e}"))?
    }

    pub async fn ended(&self) {
        let mut serving = self.serving.clone();
        while serving.changed().await.is_ok() {}
    }

    pub async fn close(mut self) -> Result<(), String> {
        let (id, thread) = (self.id, self.thread.take());
        tokio::task::spawn_blocking(move || device::stop(id).and(joined(thread)))
            .await
            .map_err(|e| format!("ublk device thread: {e}"))?
    }
}

impl Drop for Exposure {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let id = self.id;
            std::thread::spawn(move || drop(device::stop(id).and(joined(Some(thread)))));
        }
    }
}

async fn failure(thread: DeviceThread) -> String {
    tokio::task::spawn_blocking(move || joined(Some(thread)).err())
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| NOT_STARTED.to_owned())
}

fn joined(thread: Option<DeviceThread>) -> Result<(), String> {
    thread.map_or(Ok(()), |thread| {
        thread
            .join()
            .map_err(|_| "ublk device thread panicked".to_owned())
            .and_then(|served| served)
    })
}
