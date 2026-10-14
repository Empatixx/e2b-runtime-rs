use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::checkpoint::{Checkpoint, DriveState};
use crate::firecracker::{DRIVE_SLOTS, FirecrackerApi, FirecrackerProcess, MachineSize, ROOTFS_DRIVE, drive_file};
use crate::host::Host;
use crate::network::Slot;
use crate::storage::Disk;
use crate::storage::memory::write_memory_layer;

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
    memory: Option<Disk>,
    drives: Vec<AttachedDrive>,
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
    pub async fn boot(host: &Host, dir: &Path, rootfs_layers: Vec<PathBuf>, size: MachineSize) -> Result<Self> {
        let resources = Resources::acquire(host, dir, &rootfs_layers, &[], Vec::new()).await?;
        let started = start_fresh(host, dir, &resources, size).await;
        let process = resources.release_on_error(host, started).await?;
        Ok(Self {
            dir: dir.to_path_buf(),
            process,
            resources,
            rootfs_layers,
            memory_layers: Vec::new(),
        })
    }

    pub async fn restore(host: &Host, dir: &Path, checkpoint: &Checkpoint, drives: Vec<DriveState>) -> Result<Self> {
        let resources =
            Resources::acquire(host, dir, &checkpoint.rootfs_layers, &checkpoint.memory_layers, drives).await?;
        let started = start_from_checkpoint(host, dir, &resources, checkpoint).await;
        let process = resources.release_on_error(host, started).await?;
        Ok(Self {
            dir: dir.to_path_buf(),
            process,
            resources,
            rootfs_layers: checkpoint.rootfs_layers.clone(),
            memory_layers: checkpoint.memory_layers.clone(),
        })
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

    pub async fn shutdown(self, host: &Host) {
        self.process.kill().await;
        self.resources.release(host).await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    async fn capture_memory(&mut self, layer: &Path) -> Result<()> {
        let dirty = self.api().dirty_memory().await?;
        write_memory_layer(self.process.pid, &dirty, layer).await?;
        self.memory_layers.push(layer.to_path_buf());
        Ok(())
    }

    async fn seal_rootfs(&mut self, host: &Host, layer: &Path) -> Result<()> {
        host.storage.seal(&self.resources.rootfs, layer).await?;
        self.rootfs_layers.push(layer.to_path_buf());
        Ok(())
    }
}

impl Resources {
    async fn acquire(
        host: &Host,
        dir: &Path,
        rootfs_layers: &[PathBuf],
        memory_layers: &[PathBuf],
        drives: Vec<DriveState>,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let mut resources = Self {
            slot: host.network.acquire().await?,
            rootfs: host.storage.open_writable(&dir.join("rootfs"), rootfs_layers).await?,
            memory: None,
            drives: Vec::new(),
        };
        let opened = resources.open_memory_and_drives(host, dir, memory_layers, drives).await;
        resources.release_on_error(host, opened).await?;
        resources.link_drives(dir, &host.config.placeholder_drive())?;
        Ok(resources)
    }

    async fn open_memory_and_drives(
        &mut self,
        host: &Host,
        dir: &Path,
        memory_layers: &[PathBuf],
        drives: Vec<DriveState>,
    ) -> Result<()> {
        if !memory_layers.is_empty() {
            self.memory = Some(host.storage.open_read_only(&dir.join("memory"), memory_layers).await?);
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
        host.storage.close(&self.rootfs).await;
        if let Some(memory) = &self.memory {
            host.storage.close(memory).await;
        }
        for drive in &self.drives {
            host.storage.close(&drive.disk).await;
        }
    }
}

async fn start_fresh(host: &Host, dir: &Path, resources: &Resources, size: MachineSize) -> Result<FirecrackerProcess> {
    let process = FirecrackerProcess::spawn(&host.config.firecracker, &resources.slot.namespace, dir).await?;
    process
        .api
        .boot(&host.config.kernel, size)
        .await
        .context("boot firecracker")?;
    Ok(process)
}

async fn start_from_checkpoint(
    host: &Host,
    dir: &Path,
    resources: &Resources,
    checkpoint: &Checkpoint,
) -> Result<FirecrackerProcess> {
    let memory = resources.memory.as_ref().context("checkpoint without memory")?;
    let process = FirecrackerProcess::spawn(&host.config.firecracker, &resources.slot.namespace, dir).await?;
    process
        .api
        .load(&checkpoint.vm_state, &memory.device)
        .await
        .context("load snapshot")?;
    for drive in &resources.drives {
        process.api.refresh_drive(drive.state.slot).await?;
    }
    Ok(process)
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
