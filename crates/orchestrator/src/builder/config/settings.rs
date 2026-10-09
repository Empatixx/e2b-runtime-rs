use std::path::PathBuf;

use super::{BuildFlags, BuilderConfig, resolve_build_binary};
use crate::featureflags::{FlagContext, build_envd_version, build_firecracker_version, build_kernel_version};

#[derive(Debug, Clone, PartialEq)]
pub struct BuildSettings {
    pub envd: PathBuf,
    pub kernel_version: String,
    pub firecracker_version: String,
    pub envd_memory_protection: bool,
    pub flags: BuildFlags,
}

impl BuildSettings {
    pub async fn resolve(config: &BuilderConfig, template_id: &str, team_id: &str) -> Result<Self, String> {
        let contexts = [FlagContext::template(template_id), FlagContext::team(team_id)];
        let flags = BuildFlags::new(config, template_id, team_id);
        let envd_target = config.feature_flags.get(&build_envd_version(), &contexts);
        let envd = resolve_build_binary(&envd_target, &config.envd)
            .await
            .map_err(|e| format!("error resolving envd binary for build: {e}"))?;
        Ok(Self {
            envd,
            envd_memory_protection: flags.envd_memory_protection(),
            kernel_version: config
                .feature_flags
                .get(&build_kernel_version(&config.kernel_version), &contexts),
            firecracker_version: config
                .feature_flags
                .get(&build_firecracker_version(&config.firecracker_version), &contexts),
            flags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::featureflags::FeatureFlags;

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

    #[tokio::test]
    async fn versions_default_to_the_env_backed_fallbacks() {
        let settings = BuildSettings::resolve(&config(&[]), "template", "team").await.unwrap();

        assert_eq!(
            (settings.kernel_version.as_str(), settings.firecracker_version.as_str()),
            ("vmlinux-6.1.158", "v1.14-0.2.0")
        );
    }

    #[tokio::test]
    async fn an_invalid_envd_target_fails_the_build_setup() {
        let error = BuildSettings::resolve(&config(&[("build-envd-version", "../x")]), "template", "team")
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "error resolving envd binary for build: invalid build-envd-version target \"../x\""
        );
    }

    #[tokio::test]
    async fn the_flags_override_the_versions() {
        let settings = BuildSettings::resolve(
            &config(&[
                ("build-kernel-version", "vmlinux-6.1.999"),
                ("build-firecracker-version", "v1.12.1_210cbac"),
            ]),
            "template",
            "team",
        )
        .await
        .unwrap();

        assert_eq!(settings.kernel_version, "vmlinux-6.1.999");
        assert_eq!(settings.firecracker_version, "v1.12.1_210cbac");
    }
}
