use std::collections::BTreeMap;
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
use crate::memory::prefetch::source::PrefetchSettings;
use crate::network::slot::Slot;
use crate::template::metadata::{CpuTemplate, MemoryPrefetch};

const DEFAULT_INIT_PATH: &str = "/sbin/init";
const DEFAULT_VCPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u32 = 1024;
const DEFAULT_ENVD_TIMEOUT: Duration = Duration::from_secs(60);

pub type Held = Box<dyn std::any::Any + Send + Sync>;

pub type PreBoot = Box<dyn FnOnce(PathBuf) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

#[derive(Clone)]
pub enum Rootfs {
    Overlay(Arc<Overlay>),
    File(PathBuf),
}

impl From<Arc<Overlay>> for Rootfs {
    fn from(overlay: Arc<Overlay>) -> Self {
        Self::Overlay(overlay)
    }
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
    pub held: Vec<Held>,
    pub vcpus: u32,
    pub max_vcpus: u32,
    pub recorded_vm_vcpus: i64,
    pub envd_version: String,
    pub cpu_template: Option<CpuTemplate>,
    pub memory_mib: u32,
    pub work_dir: PathBuf,
    pub envd: EnvdInit,
    pub envd_timeout: Duration,
    pub prefetch: Option<MemoryPrefetch>,
    pub prefetch_settings: PrefetchSettings,
    pub use_memfd: bool,
    pub huge_pages: bool,
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

#[bon::bon]
impl SandboxConfig {
    #[builder(builder_type = SandboxBuilder, state_mod(vis = "pub"), on(String, into), on(PathBuf, into))]
    pub fn new(
        #[builder(field)] held: Vec<Held>,
        #[builder(field)] envd: EnvdInit,
        #[builder(field)] slot_ready: bool,
        id: String,
        template_id: String,
        #[builder(into)] rootfs: Rootfs,
        slot: Slot,
        firecracker: PathBuf,
        kernel: Option<PathBuf>,
        memfile: Option<Arc<BuildFile>>,
        #[builder(default = DEFAULT_INIT_PATH.to_owned())] init_path: String,
        #[builder(default)] kernel_logs: bool,
        #[builder(default = true)] kvm_clock: bool,
        #[builder(default, with = || true)] without_mmds: bool,
        io_engine: Option<String>,
        console: Option<UnboundedSender<String>>,
        #[builder(default, with = || true)] skip_envd: bool,
        memory_base: Option<Uuid>,
        pre_boot: Option<PreBoot>,
        #[builder(default = DEFAULT_VCPUS)] vcpus: u32,
        #[builder(default)] max_vcpus: u32,
        #[builder(default)] recorded_vm_vcpus: i64,
        #[builder(default)] envd_version: String,
        cpu_template: Option<CpuTemplate>,
        #[builder(default)] cmdline_args: BTreeMap<String, String>,
        #[builder(default = DEFAULT_MEMORY_MIB)] memory_mib: u32,
        work_dir: PathBuf,
        #[builder(default = DEFAULT_ENVD_TIMEOUT)] envd_timeout: Duration,
        prefetch: Option<MemoryPrefetch>,
        #[builder(default)] prefetch_settings: PrefetchSettings,
        #[builder(default = true)] use_memfd: bool,
        #[builder(default)] huge_pages: bool,
        hyperloop_ip: Option<String>,
    ) -> Self {
        Self {
            cgroup_name: format!("sbx-{id}-{}", Uuid::new_v4().simple()),
            boot: BootOptions {
                kernel_logs,
                kvm_clock,
                cmdline_args,
                ..BootOptions::new(&init_path)
            },
            mmds: !without_mmds,
            wait_for_envd: !skip_envd,
            id,
            template_id,
            rootfs,
            slot,
            slot_ready,
            firecracker,
            kernel,
            memfile,
            io_engine,
            console,
            memory_base,
            pre_boot,
            held,
            vcpus,
            max_vcpus,
            recorded_vm_vcpus,
            envd_version,
            cpu_template,
            memory_mib,
            work_dir,
            envd,
            envd_timeout,
            prefetch,
            prefetch_settings,
            use_memfd,
            huge_pages,
            hyperloop_ip,
        }
    }
}

impl<S: sandbox_builder::State> SandboxBuilder<S> {
    pub fn hold(mut self, value: impl std::any::Any + Send + Sync) -> Self {
        self.held.push(Box::new(value));
        self
    }

    pub fn envd(mut self, envd: EnvdInit) -> Self {
        self.envd = envd;
        self
    }

    pub fn pooled_slot(mut self, slot: Slot) -> SandboxBuilder<sandbox_builder::SetSlot<S>>
    where
        S::Slot: sandbox_builder::IsUnset,
    {
        self.slot_ready = true;
        self.slot(slot)
    }
}
