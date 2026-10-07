use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::block::build_file::BuildFile;
use crate::block::overlay::Overlay;
use crate::envd::EnvdInit;
use crate::network::slot::Slot;

const DEFAULT_INIT_PATH: &str = "/sbin/init";
const DEFAULT_VCPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u32 = 1024;
const DEFAULT_ENVD_TIMEOUT: Duration = Duration::from_secs(60);

pub struct SandboxConfig {
    pub id: String,
    pub template_id: String,
    pub rootfs: Arc<Overlay>,
    pub slot: Slot,
    pub firecracker: PathBuf,
    pub kernel: Option<PathBuf>,
    pub memfile: Option<Arc<BuildFile>>,
    pub init_path: String,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub work_dir: PathBuf,
    pub envd: EnvdInit,
    pub envd_timeout: Duration,
}

#[derive(Default)]
pub struct SandboxBuilder {
    id: Option<String>,
    template_id: Option<String>,
    rootfs: Option<Arc<Overlay>>,
    slot: Option<Slot>,
    firecracker: Option<PathBuf>,
    kernel: Option<PathBuf>,
    memfile: Option<Arc<BuildFile>>,
    init_path: Option<String>,
    vcpus: Option<u32>,
    memory_mib: Option<u32>,
    work_dir: Option<PathBuf>,
    envd: EnvdInit,
    envd_timeout: Option<Duration>,
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
        self.rootfs = Some(rootfs);
        self
    }

    pub fn slot(mut self, slot: Slot) -> Self {
        self.slot = Some(slot);
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

    pub fn build(self) -> Result<SandboxConfig, String> {
        Ok(SandboxConfig {
            id: self.id.ok_or("id is required")?,
            template_id: self.template_id.ok_or("template id is required")?,
            rootfs: self.rootfs.ok_or("rootfs is required")?,
            slot: self.slot.ok_or("slot is required")?,
            firecracker: self.firecracker.ok_or("firecracker is required")?,
            kernel: self.kernel,
            memfile: self.memfile,
            init_path: self.init_path.unwrap_or_else(|| DEFAULT_INIT_PATH.to_owned()),
            vcpus: self.vcpus.unwrap_or(DEFAULT_VCPUS),
            memory_mib: self.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            work_dir: self.work_dir.ok_or("work dir is required")?,
            envd: self.envd,
            envd_timeout: self.envd_timeout.unwrap_or(DEFAULT_ENVD_TIMEOUT),
        })
    }
}
