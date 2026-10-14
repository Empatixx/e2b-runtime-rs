use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::checkpoint::{Checkpoint, DriveState};
use crate::firecracker::{DRIVE_SLOTS, FirecrackerApi, FirecrackerProcess, MachineSize, ROOTFS_DRIVE, drive_file};
use crate::host::Host;
use crate::layers::Layer;
use crate::network::Slot;
use crate::storage::Disk;
use crate::storage::memory::write_memory_layer;

use super::warm_pool::WarmVm;

pub struct Vm {
    dir: PathBuf,
    process: FirecrackerProcess,
    resources: Resources,
    rootfs: Stack,
    memory: Stack,
}

#[derive(Debug, Clone, Default)]
pub struct Stack {
    pub layers: Vec<Layer>,
    pub shared: usize,
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
    pub memory: Stack,
    pub rootfs: Stack,
    pub drives: Vec<DriveState>,
}

impl Vm {
    pub async fn boot(host: &Host, warm: WarmVm, rootfs_layers: Vec<Layer>, size: MachineSize) -> Result<Self> {
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
            rootfs: Stack {
                layers: rootfs_layers,
                shared: 0,
            },
            memory: Stack::default(),
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn restore(host: &Host, warm: WarmVm, checkpoint: &Checkpoint, drives: Vec<DriveState>) -> Result<Self> {
        let vm_state = host.checkpoints.vm_state(checkpoint).await?;
        let layers = &checkpoint.rootfs_layers;
        let resources = Resources::acquire(host, &warm.dir, warm.slot, layers, Some(checkpoint), drives).await?;
        let loaded = load_checkpoint(&warm.process.api, &resources, &vm_state).await;
        resources.release_on_error(host, loaded).await?;
        Ok(Self {
            dir: warm.dir,
            process: warm.process,
            resources,
            rootfs: Stack {
                layers: checkpoint.rootfs_layers.clone(),
                shared: checkpoint.shared_rootfs_layers,
            },
            memory: Stack {
                layers: checkpoint.memory_layers.clone(),
                shared: checkpoint.shared_memory_layers,
            },
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
        self.api().pause().await?;
        self.api().save_state(&checkpoint_dir.join("vmstate")).await?;
        self.capture_memory(host, &checkpoint_dir.join("memory.commit")).await?;
        self.seal_rootfs(host, &checkpoint_dir.join("rootfs.commit")).await?;
        self.seal_drives(host, checkpoint_dir).await?;
        self.merge_own_layers(host, checkpoint_dir).await?;
        Ok(Captured {
            memory: self.memory.clone(),
            rootfs: self.rootfs.clone(),
            drives: self.mounted_drives().cloned().collect(),
        })
    }

    pub async fn resume(&self) -> Result<()> {
        self.api().resume().await
    }

    pub async fn seal_drives(&mut self, host: &Host, checkpoint_dir: &Path) -> Result<()> {
        for drive in &mut self.resources.drives {
            let sealed = sealed_drive_path(host, checkpoint_dir, &drive.state);
            let layer = host.storage.seal(&drive.disk, &sealed).await?;
            drive.state.layers.push(layer);
            host.layers
                .compact(&mut drive.state.layers, 0, &sealed.with_extension("merged"))
                .await?;
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
    async fn capture_memory(&mut self, host: &Host, sealed: &Path) -> Result<()> {
        let dirty = self.api().dirty_memory().await?;
        write_memory_layer(self.process.pid, &dirty, sealed).await?;
        self.memory.layers.push(host.storage.publish(sealed).await?);
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn seal_rootfs(&mut self, host: &Host, sealed: &Path) -> Result<()> {
        let layer = host.storage.seal(&self.resources.rootfs, sealed).await?;
        self.rootfs.layers.push(layer);
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn merge_own_layers(&mut self, host: &Host, checkpoint_dir: &Path) -> Result<()> {
        let rootfs_scratch = checkpoint_dir.join("rootfs-merged.commit");
        let memory_scratch = checkpoint_dir.join("memory-merged.commit");
        host.layers
            .compact(&mut self.rootfs.layers, self.rootfs.shared, &rootfs_scratch)
            .await?;
        host.layers
            .compact(&mut self.memory.layers, self.memory.shared, &memory_scratch)
            .await
    }
}

impl Resources {
    #[tracing::instrument(level = "debug", skip_all)]
    async fn acquire(
        host: &Host,
        dir: &Path,
        slot: Slot,
        rootfs_layers: &[Layer],
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
async fn load_checkpoint(api: &FirecrackerApi, resources: &Resources, vm_state: &Path) -> Result<()> {
    let memory = resources.memory.as_ref().context("checkpoint without memory")?;
    api.load(vm_state, &memory.disk.device).await.context("load snapshot")?;
    for drive in &resources.drives {
        api.refresh_drive(drive.state.slot).await?;
    }
    Ok(())
}

fn sealed_drive_path(host: &Host, checkpoint_dir: &Path, drive: &DriveState) -> PathBuf {
    match &drive.volume {
        Some(volume_id) => host.volumes.sealed_layer_path(volume_id),
        None => checkpoint_dir.join(format!("drive-{}.commit", drive.slot)),
    }
}

fn link(path: &Path, target: &Path) -> Result<()> {
    let _ = std::fs::remove_file(path);
    std::os::unix::fs::symlink(target, path).with_context(|| format!("link {}", path.display()))
}
