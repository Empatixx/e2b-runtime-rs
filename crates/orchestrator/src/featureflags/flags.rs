use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Flag<T> {
    pub key: &'static str,
    pub fallback: T,
}

impl<T> Flag<T> {
    pub const fn new(key: &'static str, fallback: T) -> Self {
        Self { key, fallback }
    }
}

pub type BoolFlag = Flag<bool>;
pub type IntFlag = Flag<i64>;
pub type StringFlag = Flag<String>;
pub type JsonFlag = Flag<Value>;

pub const BUILD_PROVISION_VERSION: IntFlag = Flag::new("build-provision-version", 0);
pub const BUILD_ENVD_MEMORY_PROTECTION: BoolFlag = Flag::new("build-envd-memory-protection", false);
pub const BUILD_RESERVED_DISK_SPACE_MB: IntFlag = Flag::new("build-reserved-disk-space-mb", 256);
pub const BUILD_BASE_ROOTFS_SIZE_LIMIT_MB: IntFlag = Flag::new("build-base-rootfs-size-limit-mb", 25000);
pub const BUILD_ENSURE_FREE_DISK_SPACE: BoolFlag = Flag::new("build-ensure-free-disk-space", false);
pub const BUILD_CPU_TEMPLATE: JsonFlag = Flag::new("build-cpu-template", Value::Null);
pub const FREE_PAGE_REPORTING: BoolFlag = Flag::new("free-page-reporting", false);
pub const FREE_PAGE_HINTING_CONFIG: JsonFlag = Flag::new("free-page-hinting-config", Value::Null);
pub const MAX_SANDBOXES_PER_NODE: IntFlag = Flag::new("max-sandboxes-per-node", 200);
pub const MAX_STARTING_INSTANCES_PER_NODE: IntFlag = Flag::new("max-starting-instances-per-node", 3);

pub fn build_kernel_version(fallback: &str) -> StringFlag {
    Flag::new("build-kernel-version", fallback.to_owned())
}

pub fn build_firecracker_version(fallback: &str) -> StringFlag {
    Flag::new("build-firecracker-version", fallback.to_owned())
}

pub fn build_io_engine() -> StringFlag {
    Flag::new("build-io-engine", "Sync".to_owned())
}

pub fn build_kernel_cmdline_args() -> StringFlag {
    Flag::new("build-kernel-cmdline-args", String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_fallbacks_follow_upstream() {
        assert_eq!(
            (BUILD_PROVISION_VERSION.key, BUILD_PROVISION_VERSION.fallback),
            ("build-provision-version", 0)
        );
        assert_eq!(BUILD_RESERVED_DISK_SPACE_MB.fallback, 256);
        assert_eq!(BUILD_BASE_ROOTFS_SIZE_LIMIT_MB.fallback, 25000);
        assert_eq!(MAX_SANDBOXES_PER_NODE.fallback, 200);
        assert_eq!(MAX_STARTING_INSTANCES_PER_NODE.fallback, 3);
        assert_eq!(build_io_engine().fallback, "Sync");
        assert_eq!(build_kernel_cmdline_args().key, "build-kernel-cmdline-args");
        assert_eq!(FREE_PAGE_HINTING_CONFIG.fallback, Value::Null);
        assert_eq!(
            BUILD_ENSURE_FREE_DISK_SPACE,
            Flag::new("build-ensure-free-disk-space", false)
        );
    }
}
