pub mod config;
pub mod resources;
pub mod resume;
pub mod snapshot;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use self::config::{Rootfs, SandboxBuilder, SandboxConfig};
use self::resources::Resources;
use crate::builder::config::host_arch;
use crate::cgroup::Cgroup;
use crate::envd::{self, ENVD_PORT};
use crate::exposure::nbd::expose;
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::config::VmConfig;
use crate::firecracker::launch::{GUEST_KERNEL, GUEST_ROOTFS, Launch};
use crate::firecracker::mmds::MmdsMetadata;
use crate::firecracker::process::FirecrackerProcess;
use crate::memory::prefetch::PrefetchStats;
use crate::phases::PhaseTimer;

pub struct Sandbox {
    config: SandboxConfig,
    resources: Resources,
    phases: PhaseTimer,
    prefetch_stats: Arc<PrefetchStats>,
}

impl SandboxBuilder {
    pub async fn cold_boot(self) -> Result<Sandbox, String> {
        let config = self.build()?;
        config.kernel.as_ref().ok_or("kernel is required")?;
        let mut sandbox = Sandbox::new(config);
        match sandbox.start().await {
            Ok(()) => Ok(sandbox),
            Err(e) => Err(sandbox.fail(e).await),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let leftovers = std::mem::take(&mut self.resources);
        if !leftovers.is_empty() {
            tracing::warn!(sandbox = %self.config.id, "sandbox dropped without stop; releasing in the background");
            leftovers.release_in_background(self.config.id.clone());
        }
    }
}

impl Sandbox {
    pub fn builder() -> SandboxBuilder {
        SandboxBuilder::default()
    }

    pub fn id(&self) -> &str {
        &self.config.id
    }

    pub fn envd_address(&self) -> SocketAddr {
        SocketAddr::from((self.config.slot.host_ip(), ENVD_PORT))
    }

    pub fn cgroup_path(&self) -> Option<&Path> {
        self.resources.cgroup.as_ref().map(Cgroup::path)
    }

    pub fn cgroup_stats(&self) -> Option<crate::cgroup::CgroupStats> {
        self.resources.cgroup.as_ref().map(Cgroup::stats)
    }

    pub fn phases(&self) -> String {
        self.phases.render()
    }

    pub fn faults_served(&self) -> u64 {
        self.resources
            .memory
            .as_ref()
            .map_or(0, |memory| memory.faults_served())
    }

    pub async fn died(&mut self) -> String {
        let Resources { vm, memory, disk, .. } = &mut self.resources;
        tokio::select! {
            exited = async { match vm.as_mut() { Some(vm) => vm.exited().await, None => std::future::pending().await } } => exited,
            () = async { match memory.as_ref() { Some(memory) => memory.ended().await, None => std::future::pending().await } } => {
                "the memory server stopped".to_owned()
            }
            () = async { match disk.as_ref() { Some(disk) => disk.ended().await, None => std::future::pending().await } } => {
                "the rootfs nbd server stopped".to_owned()
            }
        }
    }

    pub async fn stop(mut self) -> Result<(), String> {
        std::mem::take(&mut self.resources).release().await
    }

    fn new(mut config: SandboxConfig) -> Self {
        Self {
            resources: Resources {
                held: std::mem::take(&mut config.held),
                ..Resources::default()
            },
            config,
            phases: PhaseTimer::start(),
            prefetch_stats: Arc::default(),
        }
    }

    async fn fail(mut self, error: String) -> String {
        match std::mem::take(&mut self.resources).release().await {
            Ok(()) => error,
            Err(teardown) => format!("{error}; teardown: {teardown}"),
        }
    }

    pub async fn shut_down(mut self) -> Result<(), String> {
        let client = self.resources.vm.as_ref().ok_or("firecracker is not running")?.client();
        client.pause().await.map_err(|e| format!("failed to pause VM: {e}"))?;
        let snapfile = self.config.work_dir.join("shutdown.snapfile");
        let snapshot = client.create_snapshot(&snapfile).await;
        let _ = std::fs::remove_file(&snapfile);
        snapshot.map_err(|e| format!("error creating snapshot: {e}"))?;
        let rootfs = self.config.rootfs.clone();
        std::mem::take(&mut self.resources)
            .release()
            .await
            .map_err(|e| format!("error stopping sandbox: {e}"))?;
        match rootfs {
            Rootfs::File(path) => sync_file(path)
                .await
                .map_err(|e| format!("error stopping sandbox: {e}")),
            Rootfs::Overlay(_) => Ok(()),
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        let rootfs = self.attach_disk_and_slot().await?;
        if let Some(hook) = self.config.pre_boot.take() {
            hook(rootfs.clone())
                .await
                .map_err(|e| format!("pre-boot hook failed: {e}"))?;
        }
        let client = self.spawn_firecracker(rootfs).await?;
        self.boot(&client).await?;
        match self.config.wait_for_envd {
            true => self.wait_for_envd().await,
            false => Ok(()),
        }
    }

    async fn attach_disk_and_slot(&mut self) -> Result<PathBuf, String> {
        let overlay = match &self.config.rootfs {
            Rootfs::Overlay(overlay) => overlay.clone(),
            Rootfs::File(path) => {
                let path = path.clone();
                return self.own_slot().await.map(|()| path);
            }
        };
        let (disk, created) = tokio::join!(expose(overlay), self.own_slot());
        let exposed = disk
            .map(|exposure| self.resources.disk.insert(exposure).path())
            .map_err(|e| format!("expose rootfs: {e}"));
        created.and(exposed)
    }

    async fn own_slot(&mut self) -> Result<(), String> {
        if self.config.slot_ready {
            return Ok(());
        }
        let slot = *self.resources.slot.insert(self.config.slot);
        slot.create().await
    }

    async fn spawn_firecracker(&mut self, rootfs: PathBuf) -> Result<FirecrackerClient, String> {
        std::fs::create_dir_all(&self.config.work_dir)
            .map_err(|e| format!("create work dir {}: {e}", self.config.work_dir.display()))?;
        let cgroup = self.create_cgroup();
        let launch = Launch::builder()
            .binary(&self.config.firecracker)
            .socket(self.config.work_dir.join("firecracker.sock"))
            .namespace(&self.config.slot.namespace())
            .rootfs(rootfs)
            .kernel(self.config.kernel.clone().unwrap_or_else(|| PathBuf::from("/dev/null")))
            .cgroup(cgroup)
            .build()?;
        let vm = FirecrackerProcess::spawn(&launch).await?;
        if let Some(console) = self.config.console.clone() {
            forward_console(vm.console_lines(), console);
        }
        Ok(self.resources.vm.insert(vm).client())
    }

    fn create_cgroup(&mut self) -> Option<PathBuf> {
        Cgroup::create(&self.config.cgroup_name)
            .inspect_err(
                |e| tracing::warn!(sandbox = %self.config.id, error = %e, "could not create the sandbox cgroup"),
            )
            .ok()
            .map(|cgroup| self.resources.cgroup.insert(cgroup).path().to_owned())
    }

    async fn boot(&self, client: &FirecrackerClient) -> Result<(), String> {
        let vm_config = VmConfig::builder()
            .kernel(GUEST_KERNEL)
            .rootfs(GUEST_ROOTFS)
            .vcpus(self.config.vcpus)
            .memory_mib(self.config.memory_mib)
            .boot_args(&self.config.boot.kernel_args(host_arch()))
            .io_engine(self.config.io_engine.as_deref())
            .build()?;
        let metadata = self.config.mmds.then(|| self.mmds_metadata());
        client.boot(&vm_config, metadata.as_ref()).await
    }

    fn mmds_metadata(&self) -> MmdsMetadata {
        MmdsMetadata {
            sandbox_id: self.config.id.clone(),
            template_id: self.config.template_id.clone(),
            logs_collector_address: self
                .config
                .hyperloop_ip
                .as_ref()
                .map(|ip| format!("http://{ip}/logs"))
                .unwrap_or_default(),
            access_token_hash: Some(envd::hash_access_token(
                self.config.envd.access_token.as_deref().unwrap_or_default(),
            )),
        }
    }

    pub async fn wait_for_envd(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + self.config.envd_timeout;
        let address = self.envd_address();
        let vm = self.resources.vm.as_mut().ok_or("firecracker is not running")?;
        let initialised = tokio::select! {
            initialised = envd::init(address, &self.config.envd, deadline) => initialised,
            exited = vm.exited() => return Err(exited),
        };
        let attempts = initialised.map_err(|e| format!("{e}; console: {}", self.console_tail()))?;
        self.phases.note("envd_attempts", attempts);
        Ok(())
    }

    fn console_tail(&self) -> String {
        self.resources
            .vm
            .as_ref()
            .map(FirecrackerProcess::console_tail)
            .unwrap_or_default()
    }
}

fn forward_console(mut lines: UnboundedReceiver<String>, sink: UnboundedSender<String>) {
    tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            if sink.send(line).is_err() {
                return;
            }
        }
    });
}

async fn sync_file(path: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || std::fs::File::open(&path).and_then(|file| file.sync_all()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}
