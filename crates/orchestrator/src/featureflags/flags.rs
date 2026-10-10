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
pub const REBOOT_CPU_TEMPLATE_OVERRIDE: JsonFlag = Flag::new("reboot-cpu-template-override", Value::Null);
pub const FREE_PAGE_REPORTING: BoolFlag = Flag::new("free-page-reporting", false);
pub const FREE_PAGE_HINTING_CONFIG: JsonFlag = Flag::new("free-page-hinting-config", Value::Null);
pub const EGRESS_RETIREMENT_TIMEOUT_MS: IntFlag = Flag::new("egress-retirement-timeout-ms", 60000);
pub const MAX_SANDBOXES_PER_NODE: IntFlag = Flag::new("max-sandboxes-per-node", 200);
pub const MAX_STARTING_INSTANCES_PER_NODE: IntFlag = Flag::new("max-starting-instances-per-node", 3);
pub const SANDBOX_MAX_INCOMING_CONNECTIONS: IntFlag = Flag::new("sandbox-max-incoming-connections", -1);

pub const IN_PLACE_CHECKPOINT: BoolFlag = Flag::new("in-place-checkpoint", false);
pub const FILESYSTEM_ONLY_CHECKPOINT: BoolFlag = Flag::new("filesystem-only-checkpoint", false);
pub const IN_PLACE_CHECKPOINT_REPORTING: BoolFlag = Flag::new("in-place-checkpoint-reporting", false);
pub const DEFER_MEMORY_EXPORT: BoolFlag = Flag::new("defer-memory-export", false);
pub const FS_FREEZE_VIA_EXEC: BoolFlag = Flag::new("fsfreeze-via-exec", false);
pub const GUEST_SYNC_TIMEOUT_MS: IntFlag = Flag::new("guest-sync-timeout-milliseconds", 0);

pub const PEER_TO_PEER_CHUNK_TRANSFER: BoolFlag = Flag::new("peer-to-peer-chunk-transfer", false);
pub const PEER_TO_PEER_ASYNC_CHECKPOINT: BoolFlag = Flag::new("peer-to-peer-async-checkpoint", false);

pub const PAUSE_RESUME_PREFETCH_HARVEST: BoolFlag = Flag::new("pause-resume-prefetch-harvest", false);
pub const PAUSE_RESUME_PREFETCH_CONSUME: BoolFlag = Flag::new("pause-resume-prefetch-consume", false);
pub const PAUSE_RESUME_PREFETCH_HARVEST_TIMEOUT_MS: IntFlag =
    Flag::new("pause-resume-prefetch-harvest-timeout-ms", 15000);
pub const RESUME_LAST_CYCLE_PREFETCH_MAX_MIB: IntFlag = Flag::new("resume-last-cycle-prefetch-max-mib", -1);

pub fn resume_prefetch_source() -> StringFlag {
    Flag::new("resume-prefetch-source", "init".to_owned())
}

pub const USE_MEMFD: BoolFlag = Flag::new("use-memfd", true);
pub const MEMORY_PREFETCH_MAX_FETCH_WORKERS: IntFlag = Flag::new("memory-prefetch-max-fetch-workers", 16);
pub const MEMORY_PREFETCH_MAX_COPY_WORKERS: IntFlag = Flag::new("memory-prefetch-max-copy-workers", 8);
pub const MEMORY_PREFETCH_COALESCE_MAX_MB: IntFlag = Flag::new("memory-prefetch-coalesce-max-mb", 0);
pub const ENVD_INIT_REQUEST_TIMEOUT_MILLISECONDS: IntFlag = Flag::new("envd-init-request-timeout-milliseconds", 50);
pub const BUILD_CACHE_MAX_USAGE_PERCENTAGE: IntFlag = Flag::new("build-cache-max-usage-percentage", 85);

const DEFAULT_ENVD_TIMEOUT_MS: i64 = 10_000;
const DEFAULT_ENVD_VERSION: &str = "promoted";

pub fn envd_timeout_milliseconds() -> IntFlag {
    Flag::new("envd-timeout-milliseconds", envd_timeout_fallback_ms())
}

fn envd_timeout_fallback_ms() -> i64 {
    std::env::var("ENVD_TIMEOUT")
        .ok()
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| crate::go_time::parse_duration(&raw).ok())
        .map_or(DEFAULT_ENVD_TIMEOUT_MS, |nanos| nanos / 1_000_000)
}

pub fn build_envd_version() -> StringFlag {
    let fallback = std::env::var("DEFAULT_ENVD_VERSION")
        .ok()
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| DEFAULT_ENVD_VERSION.to_owned());
    Flag::new("build-envd-version", fallback)
}

pub fn build_kernel_version(fallback: &str) -> StringFlag {
    Flag::new("build-kernel-version", fallback.to_owned())
}

pub fn build_firecracker_version(fallback: &str) -> StringFlag {
    Flag::new("build-firecracker-version", fallback.to_owned())
}

pub fn pause_admission_disk_headroom_mib() -> IntFlag {
    let fallback = std::env::var("PAUSE_ADMISSION_DISK_HEADROOM_MIB")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(-1);
    Flag::new("pause-admission-disk-headroom-mib", fallback)
}

pub fn memfile_diff_dedup() -> JsonFlag {
    Flag::new(
        "memfile-diff-dedup",
        serde_json::json!({
            "enabled": false,
            "bestEffort": false,
            "directIO": false,
            "maxFetchWindowsPerBlock": 0,
            "maxPromotedParentPagesPerBlock": 0,
            "maxPagesPerPromotedFrame": 0,
            "blockFaultPct": 0,
            "fetchRunWindowPages": 0,
        }),
    )
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
        assert_eq!(EGRESS_RETIREMENT_TIMEOUT_MS.fallback, 60000);
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
