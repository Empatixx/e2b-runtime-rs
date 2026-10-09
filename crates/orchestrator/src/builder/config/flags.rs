use super::BuilderConfig;
use crate::featureflags::{
    BUILD_BASE_ROOTFS_SIZE_LIMIT_MB, BUILD_ENSURE_FREE_DISK_SPACE, BUILD_ENVD_MEMORY_PROTECTION,
    BUILD_PROVISION_VERSION, BUILD_RESERVED_DISK_SPACE_MB, FeatureFlags, FlagContext, build_io_engine,
};

#[derive(Debug, Clone, PartialEq)]
pub struct BuildFlags {
    flags: FeatureFlags,
    contexts: Vec<FlagContext>,
}

impl BuildFlags {
    pub fn new(config: &BuilderConfig, template_id: &str, team_id: &str) -> Self {
        Self {
            flags: config.feature_flags.clone(),
            contexts: vec![FlagContext::template(template_id), FlagContext::team(team_id)],
        }
    }

    pub fn provision_version(&self) -> Option<i64> {
        Some(self.flags.get(&BUILD_PROVISION_VERSION, &self.contexts))
            .filter(|version| *version != BUILD_PROVISION_VERSION.fallback)
    }

    pub fn envd_memory_protection(&self) -> bool {
        self.flags.get(&BUILD_ENVD_MEMORY_PROTECTION, &self.contexts)
    }

    pub fn reserved_disk_space_mb(&self) -> i64 {
        self.flags.get(&BUILD_RESERVED_DISK_SPACE_MB, &self.contexts)
    }

    pub fn base_rootfs_size_limit_mb(&self) -> i64 {
        self.flags.get(&BUILD_BASE_ROOTFS_SIZE_LIMIT_MB, &self.contexts)
    }

    pub fn io_engine(&self) -> String {
        self.flags.get(&build_io_engine(), &self.contexts)
    }

    pub fn ensure_free_disk_space(&self) -> bool {
        self.flags.get(&BUILD_ENSURE_FREE_DISK_SPACE, &self.contexts)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn flags_with(overrides: &[(&str, Value)]) -> BuildFlags {
        let builder = overrides.iter().fold(FeatureFlags::builder(), |builder, (key, value)| {
            builder.override_flag(key, value.clone())
        });
        let config = BuilderConfig {
            feature_flags: builder.offline(),
            ..BuilderConfig::from_vars(&Default::default())
        };
        BuildFlags::new(&config, "template", "team")
    }

    #[test]
    fn unset_flags_serve_the_upstream_defaults() {
        let flags = flags_with(&[]);

        assert_eq!(flags.provision_version(), None);
        assert!(!flags.envd_memory_protection());
        assert_eq!(flags.reserved_disk_space_mb(), 256);
        assert_eq!(flags.base_rootfs_size_limit_mb(), 25_000);
        assert_eq!(flags.io_engine(), "Sync");
        assert!(!flags.ensure_free_disk_space());
    }

    #[test]
    fn the_provision_version_counts_only_when_it_differs_from_the_fallback() {
        assert_eq!(
            flags_with(&[("build-provision-version", Value::from(0))]).provision_version(),
            None
        );
        assert_eq!(
            flags_with(&[("build-provision-version", Value::from(7))]).provision_version(),
            Some(7)
        );
    }

    #[test]
    fn overridden_flags_are_read() {
        let flags = flags_with(&[
            ("build-envd-memory-protection", Value::from(true)),
            ("build-reserved-disk-space-mb", Value::from(0)),
            ("build-base-rootfs-size-limit-mb", Value::from(100)),
            ("build-io-engine", Value::from("Async")),
            ("build-ensure-free-disk-space", Value::from(true)),
        ]);

        assert!(flags.envd_memory_protection());
        assert_eq!(flags.reserved_disk_space_mb(), 0);
        assert_eq!(flags.base_rootfs_size_limit_mb(), 100);
        assert_eq!(flags.io_engine(), "Async");
        assert!(flags.ensure_free_disk_space());
    }
}
