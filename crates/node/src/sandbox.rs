pub mod envd;
pub mod vm;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, RwLock};

use crate::checkpoint::{Checkpoint, DriveState, new_id};
use crate::firecracker::{DRIVE_SLOTS, Metadata};
use crate::host::Host;
use crate::template::{Template, Templates};
use envd::{Envd, VolumeMount, hash_token};
use vm::{Captured, Vm};

const REAPER_INTERVAL: Duration = Duration::from_secs(1);
const GUEST_DRIVE_NAMES: [&str; DRIVE_SLOTS] = ["/dev/vdb", "/dev/vdc"];

#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub sandbox_id: String,
    pub template_id: String,
    pub started_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub disk_mb: u64,
    pub envd_version: String,
    pub envd_token: String,
    pub auto_pause: bool,
    pub metadata: HashMap<String, String>,
    pub env_vars: HashMap<String, String>,
    pub volume_mounts: Vec<MountRequest>,
    pub paused: bool,
}

#[derive(Debug, Clone)]
pub struct MountRequest {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub template_id: String,
    pub timeout: Duration,
    pub auto_pause: bool,
    pub metadata: HashMap<String, String>,
    pub env_vars: HashMap<String, String>,
    pub volume_mounts: Vec<MountRequest>,
}

struct Sandbox {
    info: SandboxInfo,
    state: State,
}

enum State {
    Running(Box<Vm>),
    Paused(Checkpoint),
    Stopped,
}

pub struct Sandboxes {
    host: Arc<Host>,
    templates: Templates,
    entries: RwLock<HashMap<String, Arc<Mutex<Sandbox>>>>,
}

impl Sandboxes {
    pub fn new(host: Arc<Host>) -> Arc<Self> {
        let templates = Templates::new(host.config.templates_dir());
        Arc::new(Self {
            host,
            templates,
            entries: RwLock::default(),
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn create(&self, request: CreateRequest) -> Result<SandboxInfo> {
        let template = self.templates.get(&request.template_id)?;
        let checkpoint = Checkpoint::load(&self.host.config.checkpoints_dir(), &template.checkpoint)?;
        let info = new_sandbox_info(&request, &template, &checkpoint);
        let drives = self.drives_for_new_sandbox(&info, &checkpoint).await?;
        let vm = self.start_vm(&info, &checkpoint, drives).await?;
        self.insert(Sandbox {
            info: info.clone(),
            state: State::Running(vm),
        })
        .await;
        tracing::info!(
            sandbox_id = info.sandbox_id,
            template_id = info.template_id,
            "sandbox created"
        );
        Ok(info)
    }

    pub async fn get(&self, sandbox_id: &str) -> Option<SandboxInfo> {
        let sandbox = self.entry(sandbox_id).await?;
        let info = sandbox.lock().await.info.clone();
        Some(info)
    }

    pub async fn list(&self) -> Vec<SandboxInfo> {
        let entries: Vec<_> = self.entries.read().await.values().cloned().collect();
        let mut infos = Vec::with_capacity(entries.len());
        for entry in entries {
            infos.push(entry.lock().await.info.clone());
        }
        infos
    }

    pub async fn guest_address(&self, sandbox_id: &str) -> Option<std::net::Ipv4Addr> {
        let sandbox = self.entry(sandbox_id).await?;
        let sandbox = sandbox.lock().await;
        match &sandbox.state {
            State::Running(vm) => Some(vm.slot().host_ip),
            State::Paused(_) | State::Stopped => None,
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn pause(&self, sandbox_id: &str) -> Result<()> {
        let entry = self.entry(sandbox_id).await.context("sandbox not found")?;
        let sandbox = &mut *entry.lock().await;
        let State::Running(vm) = &mut sandbox.state else {
            bail!("sandbox {sandbox_id} is not running");
        };
        let captured = self.capture(vm).await?;
        let checkpoint = self.save_checkpoint(&sandbox.info, captured)?;
        self.commit_volumes(&checkpoint.drives, false).await?;
        if let State::Running(vm) = std::mem::replace(&mut sandbox.state, State::Paused(checkpoint)) {
            self.shutdown_in_background(vm);
        }
        sandbox.info.paused = true;
        tracing::info!(sandbox_id, "sandbox paused");
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn connect(&self, sandbox_id: &str, timeout: Duration) -> Result<(SandboxInfo, bool)> {
        let entry = self.entry(sandbox_id).await.context("sandbox not found")?;
        let sandbox = &mut *entry.lock().await;
        let resumed = match &sandbox.state {
            State::Running(_) => false,
            State::Paused(checkpoint) => {
                let vm = self
                    .start_vm(&sandbox.info, checkpoint, checkpoint.drives.clone())
                    .await?;
                sandbox.state = State::Running(vm);
                true
            }
            State::Stopped => bail!("sandbox {sandbox_id} is stopped"),
        };
        sandbox.info.paused = false;
        sandbox.info.end_at = sandbox.info.end_at.max(Utc::now() + timeout);
        Ok((sandbox.info.clone(), resumed))
    }

    pub async fn set_timeout(&self, sandbox_id: &str, timeout: Duration) -> Result<()> {
        let sandbox = self.entry(sandbox_id).await.context("sandbox not found")?;
        sandbox.lock().await.info.end_at = Utc::now() + timeout;
        Ok(())
    }

    pub async fn snapshot(&self, sandbox_id: &str, name: Option<String>) -> Result<String> {
        let sandbox = self.entry(sandbox_id).await.context("sandbox not found")?;
        let sandbox = &mut *sandbox.lock().await;
        let State::Running(vm) = &mut sandbox.state else {
            bail!("sandbox {sandbox_id} is paused");
        };
        let captured = self.capture(vm).await?;
        vm.resume().await?;
        self.commit_volumes(&captured.drives, false).await?;
        let checkpoint = self.save_checkpoint(&sandbox.info, detach_volumes(captured))?;
        let template_id = name.unwrap_or_else(|| new_id("snap"));
        self.templates.save(&Template {
            template_id: template_id.clone(),
            checkpoint: checkpoint.id,
        })?;
        Ok(template_id)
    }

    pub async fn kill(&self, sandbox_id: &str) -> Result<bool> {
        let Some(entry) = self.entries.write().await.remove(sandbox_id) else {
            return Ok(false);
        };
        let sandbox = &mut *entry.lock().await;
        match std::mem::replace(&mut sandbox.state, State::Stopped) {
            State::Running(vm) => self.stop_running(&sandbox.info, vm).await?,
            State::Paused(checkpoint) => self.commit_volumes(&checkpoint.drives, true).await?,
            State::Stopped => {}
        }
        tracing::info!(sandbox_id, "sandbox killed");
        Ok(true)
    }

    pub fn spawn_reaper(self: &Arc<Self>) {
        let sandboxes = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAPER_INTERVAL).await;
                sandboxes.reap_expired().await;
            }
        });
    }

    async fn reap_expired(&self) {
        let now = Utc::now();
        for info in self
            .list()
            .await
            .into_iter()
            .filter(|info| !info.paused && info.end_at < now)
        {
            tracing::info!(
                sandbox_id = info.sandbox_id,
                auto_pause = info.auto_pause,
                "sandbox timed out"
            );
            let result = match info.auto_pause {
                true => self.pause(&info.sandbox_id).await,
                false => self.kill(&info.sandbox_id).await.map(drop),
            };
            if let Err(e) = result {
                tracing::warn!(sandbox_id = info.sandbox_id, error = %e, "timeout handling failed");
            }
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn start_vm(&self, info: &SandboxInfo, checkpoint: &Checkpoint, drives: Vec<DriveState>) -> Result<Box<Vm>> {
        let dir = self
            .host
            .config
            .sandboxes_dir()
            .join(new_id(&format!("{}-", info.sandbox_id)));
        let vm = Vm::restore(&self.host, &dir, checkpoint, drives).await?;
        match self.start_session(info, &vm).await {
            Ok(()) => Ok(Box::new(vm)),
            Err(e) => {
                vm.shutdown(&self.host).await;
                Err(e)
            }
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn start_session(&self, info: &SandboxInfo, vm: &Vm) -> Result<()> {
        vm.api().set_metadata(&metadata(info)).await?;
        let envd = Envd::new(vm.slot().host_ip);
        envd.wait_ready().await?;
        let mounts = vm.mounted_drives().map(guest_mount).collect();
        envd.init(&info.envd_token, &info.env_vars, mounts).await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn capture(&self, vm: &mut Vm) -> Result<Captured> {
        let checkpoint_dir = self.host.config.checkpoints_dir().join(new_id("ckpt"));
        vm.capture(&self.host, &checkpoint_dir).await
    }

    fn save_checkpoint(&self, info: &SandboxInfo, captured: Captured) -> Result<Checkpoint> {
        let id = captured
            .vm_state
            .parent()
            .and_then(|dir| dir.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .context("checkpoint directory")?;
        let checkpoint = Checkpoint {
            id,
            vm_state: captured.vm_state,
            memory_layers: captured.memory_layers,
            rootfs_layers: captured.rootfs_layers,
            drives: captured.drives,
            vcpus: info.vcpus,
            memory_mib: info.memory_mib,
            disk_mb: info.disk_mb,
            envd_version: info.envd_version.clone(),
        };
        checkpoint.save(&self.host.config.checkpoints_dir())?;
        Ok(checkpoint)
    }

    async fn drives_for_new_sandbox(&self, info: &SandboxInfo, checkpoint: &Checkpoint) -> Result<Vec<DriveState>> {
        let mut drives = checkpoint.drives.clone();
        let mut free_slots = free_drive_slots(&drives).into_iter();
        for mount in &info.volume_mounts {
            let slot = free_slots.next().context("too many volume mounts")?;
            let volume = self.host.volumes.attach(&mount.name, &info.sandbox_id).await?;
            drives.push(DriveState {
                slot,
                mount_path: mount.path.clone(),
                volume: Some(volume.volume_id),
                layers: volume.layers,
            });
        }
        Ok(drives)
    }

    async fn commit_volumes(&self, drives: &[DriveState], detach: bool) -> Result<()> {
        for drive in drives {
            if let Some(volume_id) = &drive.volume {
                self.host
                    .volumes
                    .commit(volume_id, drive.layers.clone(), detach)
                    .await?;
            }
        }
        Ok(())
    }

    async fn stop_running(&self, info: &SandboxInfo, mut vm: Box<Vm>) -> Result<()> {
        let has_volumes = vm.mounted_drives().any(|drive| drive.volume.is_some());
        if has_volumes {
            sync_guest(info, &vm).await;
            vm.api().pause().await?;
            let scratch = vm.dir().join("final");
            vm.seal_drives(&self.host, &scratch).await?;
            let drives: Vec<DriveState> = vm.mounted_drives().cloned().collect();
            self.commit_volumes(&drives, true).await?;
        }
        self.shutdown_in_background(vm);
        Ok(())
    }

    fn shutdown_in_background(&self, vm: Box<Vm>) {
        let host = self.host.clone();
        tokio::spawn(async move { vm.shutdown(&host).await });
    }

    async fn insert(&self, sandbox: Sandbox) {
        let id = sandbox.info.sandbox_id.clone();
        self.entries.write().await.insert(id, Arc::new(Mutex::new(sandbox)));
    }

    async fn entry(&self, sandbox_id: &str) -> Option<Arc<Mutex<Sandbox>>> {
        self.entries.read().await.get(sandbox_id).cloned()
    }
}

fn new_sandbox_info(request: &CreateRequest, template: &Template, checkpoint: &Checkpoint) -> SandboxInfo {
    let now = Utc::now();
    SandboxInfo {
        sandbox_id: new_id("i"),
        template_id: template.template_id.clone(),
        started_at: now,
        end_at: now + request.timeout,
        vcpus: checkpoint.vcpus,
        memory_mib: checkpoint.memory_mib,
        disk_mb: checkpoint.disk_mb,
        envd_version: checkpoint.envd_version.clone(),
        envd_token: new_id(""),
        auto_pause: request.auto_pause,
        metadata: request.metadata.clone(),
        env_vars: request.env_vars.clone(),
        volume_mounts: request.volume_mounts.clone(),
        paused: false,
    }
}

async fn sync_guest(info: &SandboxInfo, vm: &Vm) {
    Envd::new(vm.slot().host_ip)
        .sync_disks(&info.envd_token)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                sandbox_id = info.sandbox_id,
                error = format!("{e:#}"),
                "guest sync failed"
            )
        });
}

fn free_drive_slots(drives: &[DriveState]) -> Vec<usize> {
    (0..DRIVE_SLOTS)
        .filter(|slot| drives.iter().all(|drive| drive.slot != *slot))
        .collect()
}

fn metadata(info: &SandboxInfo) -> Metadata {
    Metadata {
        sandbox_id: info.sandbox_id.clone(),
        template_id: info.template_id.clone(),
        logs_address: String::new(),
        access_token_hash: hash_token(&info.envd_token),
    }
}

fn guest_mount(drive: &DriveState) -> VolumeMount {
    VolumeMount {
        nfs_target: GUEST_DRIVE_NAMES[drive.slot].to_owned(),
        path: drive.mount_path.clone(),
    }
}

fn detach_volumes(captured: Captured) -> Captured {
    let drives = captured
        .drives
        .into_iter()
        .map(|drive| DriveState { volume: None, ..drive })
        .collect();
    Captured { drives, ..captured }
}
