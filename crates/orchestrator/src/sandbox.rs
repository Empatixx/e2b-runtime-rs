pub mod config;
pub mod resources;

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::time::Instant;

use self::config::{SandboxBuilder, SandboxConfig};
use self::resources::Resources;
use crate::cgroup::Cgroup;
use crate::envd::{self, ENVD_PORT};
use crate::exposure::nbd::expose;
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::config::VmConfig;
use crate::firecracker::mmds::MmdsMetadata;
use crate::firecracker::process::FirecrackerProcess;

pub struct Sandbox {
    config: SandboxConfig,
    resources: Resources,
}

impl SandboxBuilder {
    pub async fn cold_boot(self) -> Result<Sandbox, String> {
        let mut sandbox = Sandbox::new(self.build()?);
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

    pub async fn stop(mut self) -> Result<(), String> {
        std::mem::take(&mut self.resources).release().await
    }

    fn new(config: SandboxConfig) -> Self {
        Self {
            config,
            resources: Resources::default(),
        }
    }

    async fn fail(mut self, error: String) -> String {
        match std::mem::take(&mut self.resources).release().await {
            Ok(()) => error,
            Err(teardown) => format!("{error}; teardown: {teardown}"),
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        let rootfs = self.attach_disk_and_slot().await?;
        let client = self.spawn_firecracker().await?;
        self.boot(&client, rootfs).await?;
        self.init_envd().await
    }

    async fn attach_disk_and_slot(&mut self) -> Result<PathBuf, String> {
        let slot = *self.resources.slot.insert(self.config.slot);
        let (disk, created) = tokio::join!(expose(self.config.rootfs.clone()), slot.create());
        let exposed = disk
            .map(|exposure| self.resources.disk.insert(exposure).path())
            .map_err(|e| format!("expose rootfs: {e}"));
        created.and(exposed)
    }

    async fn spawn_firecracker(&mut self) -> Result<FirecrackerClient, String> {
        std::fs::create_dir_all(&self.config.work_dir)
            .map_err(|e| format!("create work dir {}: {e}", self.config.work_dir.display()))?;
        let cgroup = Cgroup::create(&self.config.id).map_err(|e| format!("create cgroup {}: {e}", self.config.id))?;
        let cgroup = self.resources.cgroup.insert(cgroup);
        let vm = FirecrackerProcess::spawn(
            &self.config.firecracker,
            &self.config.work_dir.join("firecracker.sock"),
            &self.config.slot.namespace(),
            &self.config.work_dir.join("console.log"),
        )
        .await?;
        let vm = self.resources.vm.insert(vm);
        vm.pid()
            .ok_or_else(|| "firecracker exited before joining its cgroup".to_owned())
            .and_then(|pid| cgroup.add(pid).map_err(|e| format!("add firecracker to cgroup: {e}")))
            .map(|()| vm.client())
    }

    async fn boot(&self, client: &FirecrackerClient, rootfs: PathBuf) -> Result<(), String> {
        let vm_config = VmConfig::builder()
            .kernel(&self.config.kernel)
            .rootfs(rootfs)
            .vcpus(self.config.vcpus)
            .memory_mib(self.config.memory_mib)
            .extra_boot_args(&format!("init={}", self.config.init_path))
            .build()?;
        client.boot(&vm_config).await?;
        client.set_mmds(&self.mmds_metadata()).await
    }

    fn mmds_metadata(&self) -> MmdsMetadata {
        MmdsMetadata {
            sandbox_id: self.config.id.clone(),
            template_id: self.config.template_id.clone(),
            logs_collector_address: String::new(),
            access_token_hash: self.config.envd.access_token.as_deref().map(envd::hash_access_token),
        }
    }

    async fn init_envd(&self) -> Result<(), String> {
        let deadline = Instant::now() + self.config.envd_timeout;
        envd::init(self.envd_address(), &self.config.envd, deadline)
            .await
            .map_err(|e| format!("{e}; console: {}", self.console_tail()))
    }

    fn console_tail(&self) -> String {
        self.resources
            .vm
            .as_ref()
            .map(FirecrackerProcess::console_tail)
            .unwrap_or_default()
    }
}
