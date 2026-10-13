use serde_json::Value;

use crate::firecracker::version::parse_firecracker_version;
use crate::template::metadata::{CpuTemplate, TemplateMetadata};

const OVERRIDE_FIELD: &str = "template";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebootCpuTemplate {
    pub template: Option<CpuTemplate>,
    pub overridden: bool,
    pub rejected: Option<String>,
}

pub fn check_reboot_cpu_template(template: Option<&CpuTemplate>, firecracker_version: &str) -> Result<(), String> {
    let Some(template) = template.filter(|template| !template.is_empty()) else {
        return Ok(());
    };
    let version = parse_firecracker_version(firecracker_version)
        .map_err(|e| format!("parse firecracker version {firecracker_version:?}: {e}"))?;
    template
        .validate(&version)
        .map_err(|e| format!("firecracker {firecracker_version}: {e}"))
}

pub fn reboot_cpu_template(meta: &TemplateMetadata, override_value: &Value) -> RebootCpuTemplate {
    let build = || meta.build_cpu_template.clone();
    let rejected = |error: String| RebootCpuTemplate {
        template: build(),
        overridden: false,
        rejected: Some(error),
    };
    let fields = match override_value {
        Value::Null => return no_override(build()),
        Value::Object(fields) => fields,
        _ => {
            return rejected("invalid CPU template override: not a JSON object".to_owned());
        }
    };
    if let Some(unknown) = fields.keys().find(|key| key.as_str() != OVERRIDE_FIELD) {
        return rejected(format!("invalid CPU template override: unknown field {unknown:?}"));
    }
    match fields.get(OVERRIDE_FIELD).filter(|template| !template.is_null()) {
        None => no_override(build()),
        Some(template) => match CpuTemplate::parse(&template.to_string()) {
            Ok(template) => RebootCpuTemplate {
                template,
                overridden: true,
                rejected: None,
            },
            Err(e) => rejected(e),
        },
    }
}

fn no_override(template: Option<CpuTemplate>) -> RebootCpuTemplate {
    RebootCpuTemplate {
        template,
        overridden: false,
        rejected: None,
    }
}

pub fn reboot_max_vcpus(requested: i64, recorded: i64) -> i64 {
    match requested {
        0 => recorded,
        requested => requested,
    }
}

pub fn reboot_allowed(meta: &TemplateMetadata, request_filesystem_boot: bool) -> bool {
    meta.filesystem_only || request_filesystem_boot
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn kvm(capability: &str) -> CpuTemplate {
        CpuTemplate {
            kvm_capabilities: vec![capability.to_owned()],
            ..CpuTemplate::default()
        }
    }

    fn tsc() -> CpuTemplate {
        CpuTemplate {
            x86_tsc_khz: 3_200_000,
            ..CpuTemplate::default()
        }
    }

    #[test]
    fn the_template_is_checked_against_the_version_the_reboot_resolved_to() {
        let error = check_reboot_cpu_template(Some(&tsc()), "v1.14-0.2.0").unwrap_err();
        assert!(error.contains("v1.14-0.2.0"), "{error}");

        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(check_reboot_cpu_template(Some(&tsc()), "v1.14-0.3.0"), Ok(()));
        }
        assert_eq!(check_reboot_cpu_template(Some(&kvm("!121")), "v1.14-0.3.0"), Ok(()));
        assert!(check_reboot_cpu_template(Some(&kvm("!121")), "not-a-version").is_err());
        assert_eq!(check_reboot_cpu_template(None, "not-a-version"), Ok(()));
        assert_eq!(
            check_reboot_cpu_template(Some(&CpuTemplate::default()), "not-a-version"),
            Ok(())
        );
    }

    #[test]
    fn the_override_replaces_the_builds_template_for_one_boot() {
        let a = kvm("!121");
        let b = kvm("!122");
        let meta = TemplateMetadata {
            build_cpu_template: Some(a.clone()),
            cpu_template: Some(b.clone()),
            ..TemplateMetadata::default()
        };
        let keeps_the_build = |got: RebootCpuTemplate, why: &str| {
            assert_eq!(got.template, Some(a.clone()), "{why}");
            assert!(!got.overridden, "{why}");
        };

        let none = reboot_cpu_template(&meta, &Value::Null);
        assert_eq!(none.rejected, None);
        keeps_the_build(none, "with no override the build's template returns");
        for raw in [json!({}), json!({"template": null})] {
            let got = reboot_cpu_template(&meta, &raw);
            assert_eq!(got.rejected, None, "{raw}");
            keeps_the_build(got, "no override");
        }

        let overridden = reboot_cpu_template(&meta, &json!({"template": {"kvm_capabilities": ["!122"]}}));
        assert_eq!(
            overridden,
            RebootCpuTemplate {
                template: Some(b),
                overridden: true,
                rejected: None
            }
        );

        let none_at_all = reboot_cpu_template(&meta, &json!({"template": {}}));
        assert_eq!(
            none_at_all,
            RebootCpuTemplate {
                template: None,
                overridden: true,
                rejected: None
            },
            "{{\"template\":{{}}}} boots with no template"
        );

        for invalid in [
            json!({"template": {"bogus": 1}}),
            json!({"kvm_capabilities": ["!122"]}),
            json!({"template": {}, "x": 1}),
            json!([]),
        ] {
            let got = reboot_cpu_template(&meta, &invalid);
            assert!(got.rejected.is_some(), "{invalid}");
            keeps_the_build(got, "an invalid override falls back to the build's template");
        }
    }

    #[test]
    fn a_request_without_max_vcpus_keeps_the_lineages_vm_size() {
        assert_eq!(reboot_max_vcpus(0, 16), 16);
        assert_eq!(reboot_max_vcpus(16, 16), 16);
        assert_eq!(
            reboot_max_vcpus(2, 16),
            2,
            "an explicit maximum may shrink a cold-booted VM"
        );
        assert_eq!(reboot_max_vcpus(32, 16), 32, "a request may grow it");
        assert_eq!(reboot_max_vcpus(0, 0), 0, "an unrecorded lineage boots as requested");
    }

    #[test]
    fn the_reboot_gate_follows_the_artifact_or_the_request() {
        let filesystem_only = TemplateMetadata {
            filesystem_only: true,
            ..TemplateMetadata::default()
        };
        let memory = TemplateMetadata::default();

        assert!(reboot_allowed(&filesystem_only, false));
        assert!(reboot_allowed(&filesystem_only, true));
        assert!(
            reboot_allowed(&memory, true),
            "explicit request cold-boots a memory snapshot"
        );
        assert!(
            !reboot_allowed(&memory, false),
            "memory snapshot without a demand keeps refusing"
        );
    }
}
