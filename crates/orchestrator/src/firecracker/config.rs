use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    pub vcpus: u32,
    pub memory_mib: u32,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub boot_args: String,
    pub io_engine: Option<String>,
}

#[derive(Debug, Default)]
pub struct VmConfigBuilder {
    vcpus: Option<u32>,
    memory_mib: Option<u32>,
    kernel: Option<PathBuf>,
    rootfs: Option<PathBuf>,
    boot_args: String,
    io_engine: Option<String>,
}

impl VmConfig {
    pub fn builder() -> VmConfigBuilder {
        VmConfigBuilder::default()
    }
}

impl VmConfigBuilder {
    pub fn vcpus(mut self, vcpus: u32) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }

    pub fn kernel(mut self, kernel: impl Into<PathBuf>) -> Self {
        self.kernel = Some(kernel.into());
        self
    }

    pub fn rootfs(mut self, rootfs: impl Into<PathBuf>) -> Self {
        self.rootfs = Some(rootfs.into());
        self
    }

    pub fn boot_args(mut self, args: &str) -> Self {
        self.boot_args = args.to_owned();
        self
    }

    pub fn io_engine(mut self, io_engine: Option<&str>) -> Self {
        self.io_engine = io_engine.map(str::to_owned);
        self
    }

    pub fn build(self) -> Result<VmConfig, String> {
        Ok(VmConfig {
            vcpus: self.vcpus.unwrap_or(1),
            memory_mib: self.memory_mib.unwrap_or(256),
            kernel: self.kernel.ok_or("kernel is required")?,
            rootfs: self.rootfs.ok_or("rootfs is required")?,
            boot_args: self.boot_args,
            io_engine: self.io_engine,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_requires_kernel_and_rootfs_and_has_defaults() {
        assert_eq!(
            VmConfig::builder().rootfs("/dev/nbd0").build().unwrap_err(),
            "kernel is required"
        );
        assert_eq!(
            VmConfig::builder().kernel("/k").build().unwrap_err(),
            "rootfs is required"
        );

        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/dev/nbd3")
            .boot_args("init=/sbin/init")
            .build()
            .unwrap();
        assert_eq!(config.vcpus, 1);
        assert_eq!(config.memory_mib, 256);
        assert_eq!(config.boot_args, "init=/sbin/init");
        assert_eq!(config.io_engine, None);
    }
}
