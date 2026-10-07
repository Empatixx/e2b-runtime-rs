pub mod config;

use std::net::SocketAddr;
use std::path::PathBuf;

use futures::future::OptionFuture;
use tokio::time::Instant;

use self::config::{SandboxBuilder, SandboxConfig};
use crate::cgroup::Cgroup;
use crate::envd::{self, ENVD_PORT};
use crate::exposure::nbd::{Exposure, expose};
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::config::VmConfig;
use crate::firecracker::mmds::MmdsMetadata;
use crate::firecracker::process::FirecrackerProcess;

pub struct Sandbox {
    config: SandboxConfig,
    disk: Option<Exposure>,
    slot_created: bool,
    cgroup: Option<Cgroup>,
    vm: Option<FirecrackerProcess>,
}

impl SandboxBuilder {
    pub async fn cold_boot(self) -> Result<Sandbox, String> {
        let mut sandbox = Sandbox::new(self.build()?);
        match sandbox.start().await {
            Ok(()) => Ok(sandbox),
            Err(e) => {
                let _ = sandbox.teardown().await;
                Err(e)
            }
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
        self.teardown().await
    }

    fn new(config: SandboxConfig) -> Self {
        Self {
            config,
            disk: None,
            slot_created: false,
            cgroup: None,
            vm: None,
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        let rootfs = self.attach_disk_and_slot().await?;
        let client = self.spawn_firecracker().await?;
        self.boot(&client, rootfs).await?;
        self.init_envd().await
    }

    async fn attach_disk_and_slot(&mut self) -> Result<PathBuf, String> {
        let (disk, slot) = tokio::join!(expose(self.config.rootfs.clone()), self.config.slot.create());
        self.slot_created = slot.is_ok();
        let exposed = disk
            .map(|exposure| self.disk.insert(exposure).path())
            .map_err(|e| format!("expose rootfs: {e}"));
        slot.and(exposed)
    }

    async fn spawn_firecracker(&mut self) -> Result<FirecrackerClient, String> {
        std::fs::create_dir_all(&self.config.work_dir)
            .map_err(|e| format!("create work dir {}: {e}", self.config.work_dir.display()))?;
        let cgroup = Cgroup::create(&self.config.id).map_err(|e| format!("create cgroup {}: {e}", self.config.id))?;
        let cgroup = self.cgroup.insert(cgroup);
        let vm = FirecrackerProcess::spawn(
            &self.config.firecracker,
            &self.config.work_dir.join("firecracker.sock"),
            &self.config.slot.namespace(),
            &self.config.work_dir.join("console.log"),
        )
        .await?;
        let vm = self.vm.insert(vm);
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
        self.vm
            .as_ref()
            .map(FirecrackerProcess::console_tail)
            .unwrap_or_default()
    }

    async fn teardown(&mut self) -> Result<(), String> {
        let results = [
            self.kill_firecracker().await,
            self.close_disk().await,
            self.remove_cgroup(),
            self.destroy_slot().await,
        ];
        let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
        errors.is_empty().then_some(()).ok_or_else(|| errors.join("; "))
    }

    async fn kill_firecracker(&mut self) -> Result<(), String> {
        OptionFuture::from(self.vm.take().map(FirecrackerProcess::kill))
            .await
            .unwrap_or(Ok(()))
    }

    async fn close_disk(&mut self) -> Result<(), String> {
        OptionFuture::from(self.disk.take().map(Exposure::close))
            .await
            .unwrap_or(Ok(()))
    }

    fn remove_cgroup(&mut self) -> Result<(), String> {
        self.cgroup.take().map_or(Ok(()), |cgroup| {
            cgroup.remove().map_err(|e| format!("remove cgroup: {e}"))
        })
    }

    async fn destroy_slot(&mut self) -> Result<(), String> {
        let created = std::mem::take(&mut self.slot_created);
        OptionFuture::from(created.then(|| self.config.slot.destroy()))
            .await
            .unwrap_or(Ok(()))
    }
}
