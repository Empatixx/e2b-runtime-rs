use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{BuildFlags, BuildVm, BuilderConfig, resolve_build_binary};
use crate::featureflags::{FlagContext, build_envd_version};
use crate::firecracker::client::BalloonCaps;
use crate::template::metadata::CpuTemplate;

#[derive(Debug, Clone, PartialEq)]
pub struct BuildSettings {
    pub envd: PathBuf,
    pub kernel_version: String,
    pub firecracker_version: String,
    pub cmdline_args: BTreeMap<String, String>,
    pub cpu_template: Option<CpuTemplate>,
    pub balloon: BalloonCaps,
    pub flags: BuildFlags,
}

impl BuildSettings {
    pub async fn resolve(
        config: &BuilderConfig,
        template_id: &str,
        team_id: &str,
        vm: BuildVm,
    ) -> Result<Self, String> {
        let contexts = [FlagContext::template(template_id), FlagContext::team(team_id)];
        let flags = BuildFlags::new(config, template_id, team_id);
        let envd_target = config.feature_flags.get(&build_envd_version(), &contexts);
        let envd = resolve_build_binary(&envd_target, &config.envd)
            .await
            .map_err(|e| format!("error resolving envd binary for build: {e}"))?;
        let BuildVm {
            kernel_version,
            firecracker_version,
            cmdline_args,
            cpu_template,
            balloon,
        } = vm;
        Ok(Self {
            envd,
            kernel_version,
            firecracker_version,
            cmdline_args,
            cpu_template,
            balloon,
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

    async fn resolve(config: &BuilderConfig) -> Result<BuildSettings, String> {
        let vm = BuildVm::resolve(config, "template", "team").map_err(|status| status.message().to_owned())?;
        BuildSettings::resolve(config, "template", "team", vm).await
    }

    #[tokio::test]
    async fn versions_default_to_the_env_backed_fallbacks() {
        let settings = resolve(&config(&[])).await.unwrap();

        assert_eq!(
            (settings.kernel_version.as_str(), settings.firecracker_version.as_str()),
            ("vmlinux-6.1.158", "v1.14-0.2.0")
        );
    }

    #[tokio::test]
    async fn an_invalid_envd_target_fails_the_build_setup() {
        let error = resolve(&config(&[("build-envd-version", "../x")])).await.unwrap_err();

        assert_eq!(
            error,
            "error resolving envd binary for build: invalid build-envd-version target \"../x\""
        );
    }

    #[tokio::test]
    async fn the_flags_override_the_versions() {
        let settings = resolve(&config(&[
            ("build-kernel-version", "vmlinux-6.1.999"),
            ("build-firecracker-version", "v1.12.1_210cbac"),
        ]))
        .await
        .unwrap();

        assert_eq!(settings.kernel_version, "vmlinux-6.1.999");
        assert_eq!(settings.firecracker_version, "v1.12.1_210cbac");
    }
}
