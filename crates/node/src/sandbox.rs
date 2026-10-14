pub mod envd;
pub mod record;
pub mod vm;
pub mod warm_pool;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::checkpoint::{Checkpoint, DriveState, checkpoint_id, new_id};
use crate::firecracker::{DRIVE_SLOTS, Metadata};
use crate::host::Host;
use crate::template::Template;
use envd::{Envd, VolumeMount, hash_token};
use record::{Claim, Owner, Records, SandboxRecord};
use vm::{Captured, Vm};
use warm_pool::WarmPool;

const REAPER_INTERVAL: Duration = Duration::from_secs(1);
const WARM_VMS: usize = 4;
const GUEST_DRIVE_NAMES: [&str; DRIVE_SLOTS] = ["/dev/vdb", "/dev/vdc"];

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub domain: String,
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub enum Location {
    Here,
    Elsewhere(Owner),
    Paused,
    Missing,
}

struct Running {
    info: SandboxInfo,
    vm: Option<Box<Vm>>,
}

pub struct Sandboxes {
    host: Arc<Host>,
    me: Owner,
    records: Records,
    warm_pool: Arc<WarmPool>,
    running: RwLock<HashMap<String, Arc<Mutex<Running>>>>,
}

impl Sandboxes {
    pub fn new(host: Arc<Host>, me: Owner) -> Arc<Self> {
        Arc::new(Self {
            records: Records::new(host.store.clone()),
            warm_pool: WarmPool::new(host.clone(), WARM_VMS),
            host,
            me,
            running: RwLock::default(),
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn create(&self, request: CreateRequest) -> Result<SandboxInfo> {
        let template = self.host.templates.get(&request.template_id).await?;
        let checkpoint = self.host.checkpoints.load(&template.checkpoint).await?;
        let info = self.new_sandbox_info(&request, &template, &checkpoint);
        let drives = self.drives_for_new_sandbox(&info, &checkpoint).await?;
        let vm = self.start_vm(&info, &checkpoint, drives).await?;
        self.records
            .save(&SandboxRecord::running(info.clone(), self.me.clone()))
            .await?;
        self.insert(info.clone(), vm).await;
        tracing::info!(
            sandbox_id = info.sandbox_id,
            template_id = info.template_id,
            "sandbox created"
        );
        Ok(info)
    }

    pub async fn locate(&self, sandbox_id: &str) -> Result<Location> {
        if self.entry(sandbox_id).await.is_some() {
            return Ok(Location::Here);
        }
        let location = match self.records.get(sandbox_id).await? {
            None => Location::Missing,
            Some(SandboxRecord { owner: None, .. }) => Location::Paused,
            Some(SandboxRecord { owner: Some(owner), .. }) if owner.node_id != self.me.node_id => {
                Location::Elsewhere(owner)
            }
            Some(_) => Location::Missing,
        };
        Ok(location)
    }

    pub async fn get(&self, sandbox_id: &str) -> Result<Option<SandboxInfo>> {
        if let Some(entry) = self.entry(sandbox_id).await {
            return Ok(Some(entry.lock().await.info.clone()));
        }
        Ok(self.records.get(sandbox_id).await?.map(|record| record.info))
    }

    pub async fn list(&self) -> Result<Vec<SandboxInfo>> {
        Ok(self
            .records
            .list()
            .await?
            .into_iter()
            .map(|record| record.info)
            .collect())
    }

    pub async fn guest_address(&self, sandbox_id: &str) -> Option<std::net::Ipv4Addr> {
        let entry = self.entry(sandbox_id).await?;
        let running = entry.lock().await;
        running.vm.as_ref().map(|vm| vm.slot().host_ip)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn pause(&self, sandbox_id: &str) -> Result<()> {
        let entry = self.entry(sandbox_id).await.context("sandbox not found")?;
        let running = &mut *entry.lock().await;
        let vm = running.vm.as_mut().context("sandbox is stopping")?;
        let checkpoint = self.capture(&running.info, vm).await?;
        self.commit_volumes(&checkpoint.drives, false).await?;
        let record = SandboxRecord::paused(running.info.clone(), checkpoint.id);
        self.records.save(&record).await?;
        self.forget(sandbox_id, running).await;
        tracing::info!(sandbox_id, "sandbox paused");
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn connect(&self, sandbox_id: &str, timeout: Duration) -> Result<(SandboxInfo, bool)> {
        if let Some(entry) = self.entry(sandbox_id).await {
            let running = &mut *entry.lock().await;
            running.info.end_at = running.info.end_at.max(Utc::now() + timeout);
            return Ok((running.info.clone(), false));
        }
        let record = match self.records.claim_paused(sandbox_id, &self.me).await? {
            Claim::Claimed(record) => record,
            Claim::Taken => bail!("sandbox {sandbox_id} is being resumed on another node"),
            Claim::Missing => bail!("sandbox {sandbox_id} not found"),
        };
        let info = self.resume_claimed(&record, timeout).await?;
        tracing::info!(sandbox_id, "sandbox resumed");
        Ok((info, true))
    }

    pub async fn set_timeout(&self, sandbox_id: &str, timeout: Duration) -> Result<()> {
        let entry = self.entry(sandbox_id).await.context("sandbox not found")?;
        entry.lock().await.info.end_at = Utc::now() + timeout;
        Ok(())
    }

    pub async fn snapshot(&self, sandbox_id: &str, name: Option<String>) -> Result<String> {
        let entry = self.entry(sandbox_id).await.context("sandbox not found")?;
        let running = &mut *entry.lock().await;
        let vm = running.vm.as_mut().context("sandbox is stopping")?;
        let checkpoint = self.capture(&running.info, vm).await?;
        vm.resume().await?;
        self.commit_volumes(&checkpoint.drives, false).await?;
        let copy = self.save_copy_without_volumes(checkpoint).await?;
        let template_id = name.unwrap_or_else(|| new_id("snap"));
        self.host
            .templates
            .save(&Template {
                template_id: template_id.clone(),
                checkpoint: copy.id,
            })
            .await?;
        Ok(template_id)
    }

    pub async fn kill(&self, sandbox_id: &str) -> Result<bool> {
        if let Some(entry) = self.entry(sandbox_id).await {
            let running = &mut *entry.lock().await;
            self.stop_running(running).await?;
            self.forget(sandbox_id, running).await;
        } else if let Some(record) = self.records.get(sandbox_id).await? {
            self.release_paused_volumes(&record).await?;
        } else {
            return Ok(false);
        }
        self.records.delete(sandbox_id).await?;
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
        for info in self.expired().await {
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

    async fn expired(&self) -> Vec<SandboxInfo> {
        let now = Utc::now();
        let entries: Vec<_> = self.running.read().await.values().cloned().collect();
        let mut expired = Vec::new();
        for entry in entries {
            let info = entry.lock().await.info.clone();
            if info.end_at < now {
                expired.push(info);
            }
        }
        expired
    }

    async fn resume_claimed(&self, record: &SandboxRecord, timeout: Duration) -> Result<SandboxInfo> {
        let resumed = async {
            let checkpoint_id = record
                .checkpoint
                .as_deref()
                .context("paused sandbox without checkpoint")?;
            let checkpoint = self.host.checkpoints.load(checkpoint_id).await?;
            let info = SandboxInfo {
                end_at: Utc::now() + timeout,
                domain: self.me.domain.clone(),
                paused: false,
                ..record.info.clone()
            };
            let vm = self.start_vm(&info, &checkpoint, checkpoint.drives.clone()).await?;
            self.records
                .save(&SandboxRecord::running(info.clone(), self.me.clone()))
                .await?;
            self.insert(info.clone(), vm).await;
            Ok(info)
        }
        .await;
        if resumed.is_err() {
            self.records
                .save(&SandboxRecord {
                    owner: None,
                    ..record.clone()
                })
                .await?;
        }
        resumed
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn start_vm(&self, info: &SandboxInfo, checkpoint: &Checkpoint, drives: Vec<DriveState>) -> Result<Box<Vm>> {
        let warm = self.warm_pool.take().await?;
        let vm = Vm::restore(&self.host, warm, checkpoint, drives).await?;
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
    async fn capture(&self, info: &SandboxInfo, vm: &mut Vm) -> Result<Checkpoint> {
        let checkpoint_dir = self.host.checkpoints.new_dir();
        let captured = vm.capture(&self.host, &checkpoint_dir).await?;
        let checkpoint = checkpoint_from(checkpoint_id(&checkpoint_dir)?, info, captured);
        self.host.checkpoints.save(&checkpoint).await?;
        Ok(checkpoint)
    }

    async fn save_copy_without_volumes(&self, checkpoint: Checkpoint) -> Result<Checkpoint> {
        let drives = checkpoint
            .drives
            .into_iter()
            .map(|drive| DriveState { volume: None, ..drive })
            .collect();
        let copy = Checkpoint { drives, ..checkpoint };
        self.host.checkpoints.save(&copy).await?;
        Ok(copy)
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

    async fn release_paused_volumes(&self, record: &SandboxRecord) -> Result<()> {
        let Some(checkpoint_id) = &record.checkpoint else {
            return Ok(());
        };
        let checkpoint = self.host.checkpoints.load(checkpoint_id).await?;
        self.commit_volumes(&checkpoint.drives, true).await
    }

    async fn stop_running(&self, running: &mut Running) -> Result<()> {
        let Some(vm) = running.vm.as_mut() else {
            return Ok(());
        };
        if vm.mounted_drives().any(|drive| drive.volume.is_some()) {
            sync_guest(&running.info, vm).await;
            vm.api().pause().await?;
            let scratch = vm.dir().join("final");
            vm.seal_drives(&self.host, &scratch).await?;
            let drives: Vec<DriveState> = vm.mounted_drives().cloned().collect();
            self.commit_volumes(&drives, true).await?;
        }
        Ok(())
    }

    async fn forget(&self, sandbox_id: &str, running: &mut Running) {
        self.running.write().await.remove(sandbox_id);
        if let Some(vm) = running.vm.take() {
            let host = self.host.clone();
            tokio::spawn(async move { vm.shutdown(&host).await });
        }
    }

    async fn insert(&self, info: SandboxInfo, vm: Box<Vm>) {
        let id = info.sandbox_id.clone();
        let running = Running { info, vm: Some(vm) };
        self.running.write().await.insert(id, Arc::new(Mutex::new(running)));
    }

    async fn entry(&self, sandbox_id: &str) -> Option<Arc<Mutex<Running>>> {
        self.running.read().await.get(sandbox_id).cloned()
    }

    fn new_sandbox_info(&self, request: &CreateRequest, template: &Template, checkpoint: &Checkpoint) -> SandboxInfo {
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
            domain: self.me.domain.clone(),
            paused: false,
        }
    }
}

fn checkpoint_from(id: String, info: &SandboxInfo, captured: Captured) -> Checkpoint {
    Checkpoint {
        id,
        memory_layers: captured.memory_layers,
        rootfs_layers: captured.rootfs_layers,
        drives: captured.drives,
        vcpus: info.vcpus,
        memory_mib: info.memory_mib,
        disk_mb: info.disk_mb,
        envd_version: info.envd_version.clone(),
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
