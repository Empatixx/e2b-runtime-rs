use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::block::build_file::BuildFile;
use crate::block::overlay::Overlay;
use futures::future::BoxFuture;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::envd::EnvdInit;
use crate::firecracker::kernel_args::BootOptions;
use crate::network::slot::Slot;
use crate::template::metadata::MemoryPrefetch;

const DEFAULT_INIT_PATH: &str = "/sbin/init";
const DEFAULT_VCPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u32 = 1024;
const DEFAULT_ENVD_TIMEOUT: Duration = Duration::from_secs(60);

pub type PreBoot = Box<dyn FnOnce(PathBuf) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

#[derive(Clone)]
pub enum Rootfs {
    Overlay(Arc<Overlay>),
    File(PathBuf),
}

pub struct SandboxConfig {
    pub id: String,
    pub template_id: String,
    pub rootfs: Rootfs,
    pub slot: Slot,
    pub slot_ready: bool,
    pub firecracker: PathBuf,
    pub kernel: Option<PathBuf>,
    pub memfile: Option<Arc<BuildFile>>,
    pub boot: BootOptions,
    pub mmds: bool,
    pub io_engine: Option<String>,
    pub console: Option<UnboundedSender<String>>,
    pub wait_for_envd: bool,
    pub memory_base: Option<Uuid>,
    pub pre_boot: Option<PreBoot>,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub work_dir: PathBuf,
    pub envd: EnvdInit,
    pub envd_timeout: Duration,
    pub prefetch: Option<MemoryPrefetch>,
    pub use_memfd: bool,
    pub hyperloop_ip: Option<String>,
    pub cgroup_name: String,
}

impl SandboxConfig {
    pub fn overlay(&self) -> Result<Arc<Overlay>, String> {
        match &self.rootfs {
            Rootfs::Overlay(overlay) => Ok(overlay.clone()),
            Rootfs::File(path) => Err(format!("rootfs {} is a plain file, not an overlay", path.display())),
        }
    }
}

#[derive(Default)]
pub struct SandboxBuilder {
    id: Option<String>,
    template_id: Option<String>,
    rootfs: Option<Rootfs>,
    slot: Option<Slot>,
    slot_ready: bool,
    firecracker: Option<PathBuf>,
    kernel: Option<PathBuf>,
    memfile: Option<Arc<BuildFile>>,
    init_path: Option<String>,
    kernel_logs: Option<bool>,
    kvm_clock: Option<bool>,
    without_mmds: bool,
    io_engine: Option<String>,
    console: Option<UnboundedSender<String>>,
    skip_envd: bool,
    memory_base: Option<Uuid>,
    pre_boot: Option<PreBoot>,
    vcpus: Option<u32>,
    memory_mib: Option<u32>,
    work_dir: Option<PathBuf>,
    envd: EnvdInit,
    envd_timeout: Option<Duration>,
    prefetch: Option<MemoryPrefetch>,
    use_memfd: Option<bool>,
    hyperloop_ip: Option<String>,
}

impl SandboxBuilder {
    pub fn id(mut self, id: &str) -> Self {
        self.id = Some(id.to_owned());
        self
    }

    pub fn template_id(mut self, template_id: &str) -> Self {
        self.template_id = Some(template_id.to_owned());
        self
    }

    pub fn rootfs(mut self, rootfs: Arc<Overlay>) -> Self {
        self.rootfs = Some(Rootfs::Overlay(rootfs));
        self
    }

    pub fn rootfs_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.rootfs = Some(Rootfs::File(path.into()));
        self
    }

    pub fn kernel_logs(mut self, kernel_logs: bool) -> Self {
        self.kernel_logs = Some(kernel_logs);
        self
    }

    pub fn kvm_clock(mut self, kvm_clock: bool) -> Self {
        self.kvm_clock = Some(kvm_clock);
        self
    }

    pub fn without_mmds(mut self) -> Self {
        self.without_mmds = true;
        self
    }

    pub fn console(mut self, console: UnboundedSender<String>) -> Self {
        self.console = Some(console);
        self
    }

    pub fn memory_base(mut self, build_id: Uuid) -> Self {
        self.memory_base = Some(build_id);
        self
    }

    pub fn pre_boot(mut self, hook: PreBoot) -> Self {
        self.pre_boot = Some(hook);
        self
    }

    pub fn skip_envd(mut self) -> Self {
        self.skip_envd = true;
        self
    }

    pub fn io_engine(mut self, io_engine: &str) -> Self {
        self.io_engine = Some(io_engine.to_owned());
        self
    }

    pub fn slot(mut self, slot: Slot) -> Self {
        self.slot = Some(slot);
        self
    }

    pub fn pooled_slot(mut self, slot: Slot) -> Self {
        self.slot = Some(slot);
        self.slot_ready = true;
        self
    }

    pub fn firecracker(mut self, firecracker: impl Into<PathBuf>) -> Self {
        self.firecracker = Some(firecracker.into());
        self
    }

    pub fn kernel(mut self, kernel: impl Into<PathBuf>) -> Self {
        self.kernel = Some(kernel.into());
        self
    }

    pub fn memfile(mut self, memfile: Arc<BuildFile>) -> Self {
        self.memfile = Some(memfile);
        self
    }

    pub fn init_path(mut self, init_path: &str) -> Self {
        self.init_path = Some(init_path.to_owned());
        self
    }

    pub fn vcpus(mut self, vcpus: u32) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }

    pub fn work_dir(mut self, work_dir: impl Into<PathBuf>) -> Self {
        self.work_dir = Some(work_dir.into());
        self
    }

    pub fn envd(mut self, envd: EnvdInit) -> Self {
        self.envd = envd;
        self
    }

    pub fn envd_timeout(mut self, envd_timeout: Duration) -> Self {
        self.envd_timeout = Some(envd_timeout);
        self
    }

    pub fn prefetch(mut self, prefetch: Option<MemoryPrefetch>) -> Self {
        self.prefetch = prefetch;
        self
    }

    pub fn use_memfd(mut self, use_memfd: bool) -> Self {
        self.use_memfd = Some(use_memfd);
        self
    }

    pub fn hyperloop_ip(mut self, hyperloop_ip: &str) -> Self {
        self.hyperloop_ip = Some(hyperloop_ip.to_owned());
        self
    }

    pub fn build(self) -> Result<SandboxConfig, String> {
        let id = self.id.ok_or("id is required")?;
        Ok(SandboxConfig {
            cgroup_name: format!("sbx-{id}-{}", uuid::Uuid::new_v4().simple()),
            id,
            template_id: self.template_id.ok_or("template id is required")?,
            rootfs: self.rootfs.ok_or("rootfs is required")?,
            slot: self.slot.ok_or("slot is required")?,
            slot_ready: self.slot_ready,
            firecracker: self.firecracker.ok_or("firecracker is required")?,
            kernel: self.kernel,
            memfile: self.memfile,
            boot: BootOptions {
                kernel_logs: self.kernel_logs.unwrap_or(false),
                kvm_clock: self.kvm_clock.unwrap_or(true),
                ..BootOptions::new(self.init_path.as_deref().unwrap_or(DEFAULT_INIT_PATH))
            },
            mmds: !self.without_mmds,
            io_engine: self.io_engine,
            console: self.console,
            wait_for_envd: !self.skip_envd,
            memory_base: self.memory_base,
            pre_boot: self.pre_boot,
            vcpus: self.vcpus.unwrap_or(DEFAULT_VCPUS),
            memory_mib: self.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            work_dir: self.work_dir.ok_or("work dir is required")?,
            envd: self.envd,
            envd_timeout: self.envd_timeout.unwrap_or(DEFAULT_ENVD_TIMEOUT),
            prefetch: self.prefetch,
            use_memfd: self.use_memfd.unwrap_or(true),
            hyperloop_ip: self.hyperloop_ip,
        })
    }
}
