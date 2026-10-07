use std::path::PathBuf;

use crate::network::slot::{GUEST_IP, TAP_IP};

const BASE_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw";
const GUEST_NETMASK: &str = "255.255.255.252";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    pub vcpus: u32,
    pub memory_mib: u32,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub extra_boot_args: String,
}

#[derive(Debug, Default)]
pub struct VmConfigBuilder {
    vcpus: Option<u32>,
    memory_mib: Option<u32>,
    kernel: Option<PathBuf>,
    rootfs: Option<PathBuf>,
    extra_boot_args: String,
}

impl VmConfig {
    pub fn builder() -> VmConfigBuilder {
        VmConfigBuilder::default()
    }

    pub fn boot_args(&self) -> String {
        let network = format!("ip={GUEST_IP}::{TAP_IP}:{GUEST_NETMASK}::eth0:off");
        [BASE_BOOT_ARGS, network.as_str(), self.extra_boot_args.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
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

    pub fn extra_boot_args(mut self, args: &str) -> Self {
        self.extra_boot_args = args.to_owned();
        self
    }

    pub fn build(self) -> Result<VmConfig, String> {
        Ok(VmConfig {
            vcpus: self.vcpus.unwrap_or(1),
            memory_mib: self.memory_mib.unwrap_or(256),
            kernel: self.kernel.ok_or("kernel is required")?,
            rootfs: self.rootfs.ok_or("rootfs is required")?,
            extra_boot_args: self.extra_boot_args,
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
            .extra_boot_args("init=/bin/sleep -- 3600")
            .build()
            .unwrap();
        assert_eq!(config.vcpus, 1);
        assert_eq!(config.memory_mib, 256);
        assert_eq!(
            config.boot_args(),
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw ip=169.254.0.21::169.254.0.22:255.255.255.252::eth0:off init=/bin/sleep -- 3600"
        );
    }
}
