use super::{BuildFlags, BuilderConfig};
use crate::featureflags::{FeatureFlags, FlagContext, build_firecracker_version, build_kernel_version};

#[derive(Debug, Clone, PartialEq)]
pub struct BuildSettings {
    pub kernel_version: String,
    pub firecracker_version: String,
    pub envd_memory_protection: bool,
    pub flags: BuildFlags,
}

impl BuildSettings {
    pub fn resolve(config: &BuilderConfig, template_id: &str, team_id: &str) -> Self {
        let contexts = [FlagContext::template(template_id), FlagContext::team(team_id)];
        let flags = BuildFlags::new(config, template_id, team_id);
        Self {
            envd_memory_protection: flags.envd_memory_protection(),
            kernel_version: config
                .feature_flags
                .get(&build_kernel_version(&config.kernel_version), &contexts),
            firecracker_version: config
                .feature_flags
                .get(&build_firecracker_version(&config.firecracker_version), &contexts),
            flags,
        }
    }

    pub fn offline(config: &BuilderConfig) -> Self {
        Self::resolve(
            &BuilderConfig {
                feature_flags: FeatureFlags::offline(),
                ..config.clone()
            },
            "",
            "",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(overrides: &[(&str, &str)]) -> BuilderConfig {
        let flags = overrides
            .iter()
            .fold(FeatureFlags::builder(), |builder, (key, value)| {
                builder.override_flag(key, *value)
            })
            .offline();
        BuilderConfig {
            feature_flags: flags,
            ..BuilderConfig::from_vars(&Default::default())
        }
    }

    #[test]
    fn versions_default_to_the_env_backed_fallbacks() {
        let settings = BuildSettings::resolve(&config(&[]), "template", "team");

        assert_eq!(
            (settings.kernel_version.as_str(), settings.firecracker_version.as_str()),
            ("vmlinux-6.1.158", "v1.14-0.2.0")
        );
    }

    #[test]
    fn the_flags_override_the_versions() {
        let settings = BuildSettings::resolve(
            &config(&[
                ("build-kernel-version", "vmlinux-6.1.999"),
                ("build-firecracker-version", "v1.12.1_210cbac"),
            ]),
            "template",
            "team",
        );

        assert_eq!(settings.kernel_version, "vmlinux-6.1.999");
        assert_eq!(settings.firecracker_version, "v1.12.1_210cbac");
    }
}
