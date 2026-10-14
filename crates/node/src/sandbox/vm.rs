use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::checkpoint::{Checkpoint, DriveState};
use crate::firecracker::{DRIVE_SLOTS, FirecrackerApi, FirecrackerProcess, MachineSize, ROOTFS_DRIVE, drive_file};
use crate::host::Host;
use crate::network::Slot;
use crate::storage::Disk;
use crate::storage::memory::write_memory_layer;

use super::warm_pool::WarmVm;

pub struct Vm {
    dir: PathBuf,
    process: FirecrackerProcess,
    resources: Resources,
    rootfs_layers: Vec<PathBuf>,
    memory_layers: Vec<PathBuf>,
}

struct Resources {
    slot: Slot,
    rootfs: Disk,
    memory: Option<SharedMemoryLease>,
    drives: Vec<AttachedDrive>,
}

struct SharedMemoryLease {
    checkpoint_id: String,
    disk: Disk,
}

struct AttachedDrive {
    state: DriveState,
    disk: Disk,
}

pub struct Captured {
    pub vm_state: PathBuf,
    pub memory_layers: Vec<PathBuf>,
    pub rootfs_layers: Vec<PathBuf>,
    pub drives: Vec<DriveState>,
}

impl Vm {
    pub async fn boot(host: &Host, warm: WarmVm, rootfs_layers: Vec<PathBuf>, size: MachineSize) -> Result<Self> {
        let resources = Resources::acquire(host, &warm.dir, warm.slot, &rootfs_layers, None, Vec::new()).await?;
        let booted = warm
            .process
            .api
            .boot(&host.config.kernel, size)
            .await
            .context("boot firecracker");
        resources.release_on_error(host, booted).await?;
        Ok(Self {
            dir: warm.dir,
            process: warm.process,
            resources,
            rootfs_layers,
            memory_layers: Vec::new(),
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn restore(host: &Host, warm: WarmVm, checkpoint: &Checkpoint, drives: Vec<DriveState>) -> Result<Self> {
        let layers = &checkpoint.rootfs_layers;
        let resources = Resources::acquire(host, &warm.dir, warm.slot, layers, Some(checkpoint), drives).await?;
        let loaded = load_checkpoint(&warm.process.api, &resources, checkpoint).await;
        resources.release_on_error(host, loaded).await?;
        Ok(Self {
            dir: warm.dir,
            process: warm.process,
            resources,
            rootfs_layers: checkpoint.rootfs_layers.clone(),
            memory_layers: checkpoint.memory_layers.clone(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn slot(&self) -> &Slot {
        &self.resources.slot
    }

    pub fn api(&self) -> &FirecrackerApi {
        &self.process.api
    }

    pub fn mounted_drives(&self) -> impl Iterator<Item = &DriveState> {
        self.resources.drives.iter().map(|drive| &drive.state)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn capture(&mut self, host: &Host, checkpoint_dir: &Path) -> Result<Captured> {
        std::fs::create_dir_all(checkpoint_dir)?;
        let vm_state = checkpoint_dir.join("vmstate");
        self.api().pause().await?;
        self.api().save_state(&vm_state).await?;
        self.capture_memory(&checkpoint_dir.join("memory.commit")).await?;
        self.seal_rootfs(host, &checkpoint_dir.join("rootfs.commit")).await?;
        self.seal_drives(host, checkpoint_dir).await?;
        Ok(Captured {
            vm_state,
            memory_layers: self.memory_layers.clone(),
            rootfs_layers: self.rootfs_layers.clone(),
            drives: self.mounted_drives().cloned().collect(),
        })
    }

    pub async fn resume(&self) -> Result<()> {
        self.api().resume().await
    }

    pub async fn seal_drives(&mut self, host: &Host, checkpoint_dir: &Path) -> Result<()> {
        for drive in &mut self.resources.drives {
            let layer = next_drive_layer(host, checkpoint_dir, &drive.state);
            host.storage.seal(&drive.disk, &layer).await?;
            drive.state.layers.push(layer);
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn shutdown(self, host: &Host) {
        self.process.kill().await;
        self.resources.release(host).await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn capture_memory(&mut self, layer: &Path) -> Result<()> {
        let dirty = self.api().dirty_memory().await?;
        write_memory_layer(self.process.pid, &dirty, layer).await?;
        self.memory_layers.push(layer.to_path_buf());
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn seal_rootfs(&mut self, host: &Host, layer: &Path) -> Result<()> {
        host.storage.seal(&self.resources.rootfs, layer).await?;
        self.rootfs_layers.push(layer.to_path_buf());
        Ok(())
    }
}

impl Resources {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn acquire(
        host: &Host,
        dir: &Path,
        slot: Slot,
        rootfs_layers: &[PathBuf],
        memory_from: Option<&Checkpoint>,
        drives: Vec<DriveState>,
    ) -> Result<Self> {
        let rootfs = match host.storage.open_writable(&dir.join("rootfs"), rootfs_layers).await {
            Ok(rootfs) => rootfs,
            Err(e) => {
                host.network.release(slot).await;
                return Err(e);
            }
        };
        let mut resources = Self {
            slot,
            rootfs,
            memory: None,
            drives: Vec::new(),
        };
        let opened = resources.open_memory_and_drives(host, dir, memory_from, drives).await;
        resources.release_on_error(host, opened).await?;
        resources.link_drives(dir, &host.config.placeholder_drive())?;
        Ok(resources)
    }

    async fn open_memory_and_drives(
        &mut self,
        host: &Host,
        dir: &Path,
        memory_from: Option<&Checkpoint>,
        drives: Vec<DriveState>,
    ) -> Result<()> {
        if let Some(checkpoint) = memory_from {
            let disk = host.memory.acquire(&checkpoint.id, &checkpoint.memory_layers).await?;
            self.memory = Some(SharedMemoryLease {
                checkpoint_id: checkpoint.id.clone(),
                disk,
            });
        }
        for state in drives {
            let disk = host
                .storage
                .open_writable(&dir.join(format!("drive-{}", state.slot)), &state.layers)
                .await?;
            self.drives.push(AttachedDrive { state, disk });
        }
        Ok(())
    }

    fn link_drives(&self, dir: &Path, placeholder: &Path) -> Result<()> {
        link(&dir.join(ROOTFS_DRIVE), &self.rootfs.device)?;
        for slot in 0..DRIVE_SLOTS {
            let target = self
                .drive_in_slot(slot)
                .map_or(placeholder, |drive| drive.disk.device.as_path());
            link(&dir.join(drive_file(slot)), target)?;
        }
        Ok(())
    }

    fn drive_in_slot(&self, slot: usize) -> Option<&AttachedDrive> {
        self.drives.iter().find(|drive| drive.state.slot == slot)
    }

    async fn release_on_error<T>(&self, host: &Host, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.close_disks(host).await;
            host.network.release(self.slot.clone()).await;
        }
        result
    }

    async fn release(self, host: &Host) {
        self.close_disks(host).await;
        host.network.release(self.slot).await;
    }

    async fn close_disks(&self, host: &Host) {
        host.storage.release_writable(&self.rootfs).await;
        if let Some(memory) = &self.memory {
            host.memory.release(&memory.checkpoint_id).await;
        }
        for drive in &self.drives {
            host.storage.release_writable(&drive.disk).await;
        }
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn load_checkpoint(api: &FirecrackerApi, resources: &Resources, checkpoint: &Checkpoint) -> Result<()> {
    let memory = resources.memory.as_ref().context("checkpoint without memory")?;
    api.load(&checkpoint.vm_state, &memory.disk.device)
        .await
        .context("load snapshot")?;
    for drive in &resources.drives {
        api.refresh_drive(drive.state.slot).await?;
    }
    Ok(())
}

fn next_drive_layer(host: &Host, checkpoint_dir: &Path, drive: &DriveState) -> PathBuf {
    match &drive.volume {
        Some(volume_id) => host.volumes.layer_path(volume_id, drive.layers.len()),
        None => checkpoint_dir.join(format!("drive-{}.commit", drive.slot)),
    }
}

fn link(path: &Path, target: &Path) -> Result<()> {
    let _ = std::fs::remove_file(path);
    std::os::unix::fs::symlink(target, path).with_context(|| format!("link {}", path.display()))
}
