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
pub const STORAGE_CONCURRENT_UPLOAD_LIMIT: IntFlag = Flag::new("gcloud-concurrent-upload-limit", 8);
pub const STORAGE_MAX_UPLOAD_TASKS: IntFlag = Flag::new("gcloud-max-tasks", 16);
pub const MAX_SANDBOXES_PER_NODE: IntFlag = Flag::new("max-sandboxes-per-node", 200);
pub const MAX_STARTING_INSTANCES_PER_NODE: IntFlag = Flag::new("max-starting-instances-per-node", 3);
pub const SANDBOX_MAX_INCOMING_CONNECTIONS: IntFlag = Flag::new("sandbox-max-incoming-connections", -1);

pub const USE_SYNC_WP: BoolFlag = Flag::new("use-sync-wp", false);
pub const PREBOOT_FS_RECOVERY: BoolFlag = Flag::new("preboot-fs-recovery", false);
pub const FREEZE_USER_CGROUP_TIMEOUT_MS: IntFlag = Flag::new("freeze-user-cgroup-timeout-ms", 2000);
pub const FREEZE_GUEST_HIERARCHY: BoolFlag = Flag::new("freeze-guest-hierarchy", false);
pub const FREEZE_GUEST_HIERARCHY_MAX_CGROUPS: IntFlag = Flag::new("freeze-guest-hierarchy-max-cgroups", 512);
pub const COLLAPSE_ENVD_HEAP: BoolFlag = Flag::new("collapse-envd-heap", false);
pub const COLLAPSE_ENVD_HEAP_TIMEOUT_MS: IntFlag = Flag::new("collapse-envd-heap-timeout-ms", 10000);
pub const GUEST_PAUSE_RECLAIM: JsonFlag = Flag::new("guest-pause-reclaim", Value::Null);
pub const SYNC_WP_TRACKER_DIRTY: BoolFlag = Flag::new("sync-wp-tracker-dirty", false);
pub const MEMFD_BACKGROUND_COPY: BoolFlag = Flag::new("memfd-background-copy", true);
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

pub const PAUSE_ADMISSION_GRACE_MS: IntFlag = Flag::new("pause-admission-grace-milliseconds", -1);

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

pub fn envd_upgrade_target() -> StringFlag {
    Flag::new("envd-upgrade-target", env_or("ENVD_UPGRADE_TARGET", "off"))
}

pub fn envd_offline_upgrade_target() -> StringFlag {
    Flag::new(
        "envd-offline-upgrade-target",
        env_or("ENVD_OFFLINE_UPGRADE_TARGET", "off"),
    )
}

pub fn freeze_user_cgroup() -> BoolFlag {
    let development = matches!(env_or("ENVIRONMENT", "prod").as_str(), "dev" | "local");
    Flag::new("freeze-user-cgroup", development)
}

pub fn envd_binary_cache() -> BoolFlag {
    let development = matches!(env_or("ENVIRONMENT", "prod").as_str(), "dev" | "local");
    Flag::new(
        "envd-binary-cache",
        bool_or(&env_or("ENVD_BINARY_CACHE", ""), development),
    )
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn bool_or(raw: &str, fallback: bool) -> bool {
    match raw.is_empty() {
        true => fallback,
        false => crate::go_strconv::parse_bool(raw).unwrap_or(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bool_env_value_overrides_the_fallback_only_when_it_parses() {
        for (name, raw, fallback, expected) in [
            ("unset keeps the fallback", "", true, true),
            ("unset keeps a false fallback", "", false, false),
            ("true overrides a false fallback", "true", false, true),
            ("false overrides a true fallback", "false", true, false),
            ("1 is true", "1", false, true),
            ("0 is false", "0", true, false),
            ("garbage keeps the fallback", "yes-please", true, true),
        ] {
            assert_eq!(bool_or(raw, fallback), expected, "{name}");
        }
    }

    #[test]
    fn the_envd_upgrade_flags_follow_upstream_keys() {
        assert_eq!(envd_upgrade_target().key, "envd-upgrade-target");
        assert_eq!(envd_offline_upgrade_target().key, "envd-offline-upgrade-target");
        assert_eq!(envd_binary_cache().key, "envd-binary-cache");
    }

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
