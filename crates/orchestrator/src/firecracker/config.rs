use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct VmConfig {
    #[builder(default = 1)]
    pub vcpus: u32,
    #[builder(default = 256)]
    pub memory_mib: u32,
    #[builder(into)]
    pub kernel: PathBuf,
    #[builder(into)]
    pub rootfs: PathBuf,
    #[builder(default, into)]
    pub boot_args: String,
    #[builder(into)]
    pub io_engine: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_has_defaults() {
        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/dev/nbd3")
            .boot_args("init=/sbin/init")
            .build();
        assert_eq!(config.vcpus, 1);
        assert_eq!(config.memory_mib, 256);
        assert_eq!(config.boot_args, "init=/sbin/init");
        assert_eq!(config.io_engine, None);
    }
}
