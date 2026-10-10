mod client;
mod contexts;
mod flags;
mod log_routing;
mod prefetch;
#[cfg(test)]
mod testing;

pub use client::feature_flags_builder;
pub use client::{FeatureFlags, FeatureFlagsBuilder, FlagSettings, FlagValue, INIT_TIMEOUT, LAUNCH_DARKLY_API_KEY};
pub use contexts::{FlagContext, FlagContextBuilder, flag_context_builder};
pub use flags::{
    BUILD_BASE_ROOTFS_SIZE_LIMIT_MB, BUILD_CACHE_MAX_USAGE_PERCENTAGE, BUILD_CPU_TEMPLATE,
    BUILD_ENSURE_FREE_DISK_SPACE, BUILD_ENVD_MEMORY_PROTECTION, BUILD_PROVISION_VERSION, BUILD_RESERVED_DISK_SPACE_MB,
    COLLAPSE_ENVD_HEAP, COLLAPSE_ENVD_HEAP_TIMEOUT_MS, DEFER_MEMORY_EXPORT, EGRESS_RETIREMENT_TIMEOUT_MS,
    ENVD_INIT_REQUEST_TIMEOUT_MILLISECONDS, FILESYSTEM_ONLY_CHECKPOINT, FREE_PAGE_HINTING_CONFIG, FREE_PAGE_REPORTING,
    FREEZE_GUEST_HIERARCHY, FREEZE_GUEST_HIERARCHY_MAX_CGROUPS, FREEZE_USER_CGROUP_TIMEOUT_MS, FS_FREEZE_VIA_EXEC,
    GUEST_PAUSE_RECLAIM, GUEST_SYNC_TIMEOUT_MS, IN_PLACE_CHECKPOINT, IN_PLACE_CHECKPOINT_REPORTING,
    MAX_SANDBOXES_PER_NODE, MAX_STARTING_INSTANCES_PER_NODE, MEMFD_BACKGROUND_COPY, MEMORY_PREFETCH_COALESCE_MAX_MB,
    MEMORY_PREFETCH_MAX_COPY_WORKERS, MEMORY_PREFETCH_MAX_FETCH_WORKERS, PAUSE_ADMISSION_GRACE_MS,
    PAUSE_RESUME_PREFETCH_CONSUME, PAUSE_RESUME_PREFETCH_HARVEST, PAUSE_RESUME_PREFETCH_HARVEST_TIMEOUT_MS,
    PEER_TO_PEER_ASYNC_CHECKPOINT, PEER_TO_PEER_CHUNK_TRANSFER, PREBOOT_FS_RECOVERY, REBOOT_CPU_TEMPLATE_OVERRIDE,
    RESUME_LAST_CYCLE_PREFETCH_MAX_MIB, SANDBOX_MAX_INCOMING_CONNECTIONS, STORAGE_CONCURRENT_UPLOAD_LIMIT,
    STORAGE_MAX_UPLOAD_TASKS, SYNC_WP_TRACKER_DIRTY, USE_MEMFD, USE_SYNC_WP, build_envd_version,
    build_firecracker_version, build_io_engine, build_kernel_cmdline_args, build_kernel_version, envd_binary_cache,
    envd_offline_upgrade_target, envd_timeout_milliseconds, envd_upgrade_target, freeze_user_cgroup,
    memfile_diff_dedup, pause_admission_disk_headroom_mib, resume_prefetch_source,
};
pub use flags::{BoolFlag, Flag, IntFlag, JsonFlag, StringFlag};
pub use log_routing::{
    LogWriteConfig, LogWriteConfigResolver, LogWriteDefaults, MODE_PRIMARY_AND_SHADOW, MODE_PRIMARY_ONLY,
    logs_write_config, resolve_log_write_config,
};
pub use prefetch::ScopedPrefetchFlags;
