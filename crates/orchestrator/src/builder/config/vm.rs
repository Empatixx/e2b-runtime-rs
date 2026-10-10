use std::collections::BTreeMap;

use serde_json::Value;
use tonic::Status;

use super::BuilderConfig;
use crate::featureflags::{
    BUILD_CPU_TEMPLATE, FREE_PAGE_HINTING_CONFIG, FREE_PAGE_REPORTING, FlagContext, build_firecracker_version,
    build_kernel_cmdline_args, build_kernel_version,
};
use crate::firecracker::client::BalloonCaps;
use crate::firecracker::kernel_args::{parse_cmdline_args, render, validate_cmdline_args};
use crate::firecracker::version::{
    FirecrackerVersion, parse_firecracker_version, supports_free_page_hinting, supports_free_page_reporting,
};
use crate::go_strconv::quote;
use crate::telemetry::build::{record_build_cmdline_args, record_build_cpu_template};
use crate::template::metadata::{CpuTemplate, applied_digest};

const CPU_TEMPLATE_APPLIED: &str = "applied";
const CPU_TEMPLATE_REJECTED: &str = "rejected";
const CPU_TEMPLATE_NONE: &str = "none";
const CPU_TEMPLATE_MALFORMED: &str = "malformed";
const INVALID_BUILD_CONFIGURATION: &str = "invalid build configuration, contact support";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuildVm {
    pub kernel_version: String,
    pub firecracker_version: String,
    pub cmdline_args: BTreeMap<String, String>,
    pub cpu_template: Option<CpuTemplate>,
    pub balloon: BalloonCaps,
}

struct AppliedCpuTemplate {
    template: Option<CpuTemplate>,
    result: &'static str,
}

impl BuildVm {
    pub fn resolve(config: &BuilderConfig, template_id: &str, team_id: &str) -> Result<Self, Status> {
        let contexts = [FlagContext::template(template_id), FlagContext::team(team_id)];
        let flags = &config.feature_flags;
        let kernel_version = flags.get(&build_kernel_version(&config.kernel_version), &contexts);
        let firecracker_version = flags.get(&build_firecracker_version(&config.firecracker_version), &contexts);
        let cmdline_args = applied_cmdline_args(&flags.get(&build_kernel_cmdline_args(), &contexts));
        let version = parse_firecracker_version(&firecracker_version).map_err(|e| {
            Status::unknown(format!(
                "invalid resolved firecracker version {}: {e}",
                quote(&firecracker_version)
            ))
        })?;
        let cpu_template = applied_cpu_template(
            &flags.get(&BUILD_CPU_TEMPLATE, &contexts),
            &version,
            &firecracker_version,
        )?;
        let balloon = BalloonCaps {
            reporting: supports_free_page_reporting(&firecracker_version) && flags.get(&FREE_PAGE_REPORTING, &contexts),
            hinting: supports_free_page_hinting(&firecracker_version)
                && hinting_enabled(&flags.get(&FREE_PAGE_HINTING_CONFIG, &contexts)),
        };
        record_build_cmdline_args(&render(&cmdline_args));
        record_build_cpu_template(&applied_digest(cpu_template.template.as_ref()), cpu_template.result);
        Ok(Self {
            kernel_version,
            firecracker_version,
            cmdline_args,
            cpu_template: cpu_template.template,
            balloon,
        })
    }
}

fn applied_cmdline_args(fragment: &str) -> BTreeMap<String, String> {
    parse_cmdline_args(fragment)
        .and_then(|args| validate_cmdline_args(&args).map(|()| args))
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "rejected guest kernel cmdline args, using the default");
            BTreeMap::new()
        })
}

fn applied_cpu_template(
    flag: &Value,
    version: &FirecrackerVersion,
    firecracker_version: &str,
) -> Result<AppliedCpuTemplate, Status> {
    let parsed = CpuTemplate::parse(&flag.to_string()).map_err(|e| {
        tracing::error!(flag = BUILD_CPU_TEMPLATE.key, error = %e, "invalid build CPU template flag");
        record_build_cpu_template("", CPU_TEMPLATE_MALFORMED);
        Status::internal(INVALID_BUILD_CONFIGURATION)
    })?;
    Ok(match parsed {
        None => AppliedCpuTemplate {
            template: None,
            result: CPU_TEMPLATE_NONE,
        },
        Some(template) => match template.validate(version) {
            Ok(()) => AppliedCpuTemplate {
                template: Some(template),
                result: CPU_TEMPLATE_APPLIED,
            },
            Err(e) => {
                tracing::warn!(error = %format!("firecracker {firecracker_version}: {e}"), "rejected build CPU template, using none");
                AppliedCpuTemplate {
                    template: None,
                    result: CPU_TEMPLATE_REJECTED,
                }
            }
        },
    })
}

fn hinting_enabled(config: &Value) -> bool {
    config.get("enabled").and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tonic::Code;

    use super::*;
    use crate::featureflags::FeatureFlags;

    fn resolve(overrides: &[(&str, Value)]) -> Result<BuildVm, Status> {
        let flags = overrides
            .iter()
            .fold(FeatureFlags::builder(), |builder, (key, value)| {
                builder.override_flag(key, value.clone())
            })
            .offline();
        let config = BuilderConfig {
            feature_flags: flags,
            ..BuilderConfig::from_vars(&Default::default())
        };
        BuildVm::resolve(&config, "template", "team")
    }

    #[test]
    fn unset_flags_boot_the_default_vm() {
        let vm = resolve(&[]).unwrap();

        assert_eq!(
            vm,
            BuildVm {
                kernel_version: "vmlinux-6.1.158".to_owned(),
                firecracker_version: "v1.14-0.2.0".to_owned(),
                ..BuildVm::default()
            }
        );
    }

    #[test]
    fn valid_cmdline_args_are_applied_and_rejected_ones_fall_back_to_none() {
        let applied = resolve(&[("build-kernel-cmdline-args", json!("psi=1 nokaslr"))]).unwrap();
        assert_eq!(
            applied.cmdline_args,
            BTreeMap::from([
                ("nokaslr".to_owned(), String::new()),
                ("psi".to_owned(), "1".to_owned())
            ])
        );
        for fragment in ["psi=1 init=/evil", "=x"] {
            let vm = resolve(&[("build-kernel-cmdline-args", json!(fragment))]).unwrap();
            assert!(vm.cmdline_args.is_empty(), "fragment {fragment:?}");
        }
    }

    #[test]
    fn a_cpu_template_the_firecracker_accepts_is_applied() {
        let vm = resolve(&[("build-cpu-template", json!({"kvm_capabilities": ["!121"]}))]).unwrap();

        assert_eq!(
            vm.cpu_template,
            Some(CpuTemplate {
                kvm_capabilities: vec!["!121".to_owned()],
                ..CpuTemplate::default()
            })
        );
    }

    #[test]
    fn a_cpu_template_the_firecracker_cannot_apply_falls_back_to_none() {
        let vm = resolve(&[("build-cpu-template", json!({"x86_tsc_khz": 3_200_000}))]).unwrap();

        assert_eq!(vm.cpu_template, None);
    }

    #[test]
    fn a_malformed_cpu_template_fails_the_request_without_details() {
        let status = resolve(&[("build-cpu-template", json!([1]))]).unwrap_err();

        assert_eq!(
            (status.code(), status.message()),
            (Code::Internal, "invalid build configuration, contact support")
        );
    }

    #[test]
    fn balloon_features_follow_their_flags_on_a_firecracker_that_has_them() {
        let vm = resolve(&[
            ("free-page-reporting", json!(true)),
            ("free-page-hinting-config", json!({"enabled": true})),
        ])
        .unwrap();
        assert_eq!(
            vm.balloon,
            BalloonCaps {
                reporting: true,
                hinting: true
            }
        );

        let old = resolve(&[
            ("build-firecracker-version", json!("v1.12.1_a41d3fb")),
            ("free-page-reporting", json!(true)),
            ("free-page-hinting-config", json!({"enabled": true})),
        ])
        .unwrap();
        assert_eq!(old.balloon, BalloonCaps::default());

        let not_a_bool = resolve(&[("free-page-hinting-config", json!({"enabled": "yes"}))]).unwrap();
        assert!(!not_a_bool.balloon.hinting);
    }

    #[test]
    fn an_unparsable_firecracker_version_fails_the_request() {
        let status = resolve(&[("build-firecracker-version", json!("latest"))]).unwrap_err();

        assert_eq!(
            (status.code(), status.message()),
            (
                Code::Unknown,
                "invalid resolved firecracker version \"latest\": invalid firecracker version \"latest\": Invalid Semantic Version"
            )
        );
    }
}
