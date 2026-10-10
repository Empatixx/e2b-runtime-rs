use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod cpu_template;

pub use cpu_template::CpuTemplate;

pub const CURRENT_VERSION: u64 = 2;
const DEPRECATED_VERSION: u64 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TemplateMetadata {
    pub version: u64,
    #[serde(default)]
    pub template: TemplateVersions,
    #[serde(default)]
    pub context: Context,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Start>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_template: Option<FromTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetch: Option<Prefetch>,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub cmdline_args: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balloon: Option<Balloon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_template: Option<CpuTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cpu_template: Option<CpuTemplate>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vcpu_count: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub filesystem_only: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fs_quiesced: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateVersions {
    pub build_id: String,
    pub kernel_version: String,
    pub firecracker_version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FromTemplate {
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub build_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    #[serde(default)]
    pub start_command: String,
    #[serde(default)]
    pub ready_command: String,
    #[serde(default)]
    pub context: Context,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balloon {
    #[serde(default)]
    pub reporting: bool,
    #[serde(default)]
    pub hinting: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "is_nil_or_empty")]
    pub env_vars: Option<BTreeMap<String, String>>,
}

fn is_zero(count: &i64) -> bool {
    *count == 0
}

fn is_nil_or_empty(env_vars: &Option<BTreeMap<String, String>>) -> bool {
    env_vars.as_ref().is_none_or(BTreeMap::is_empty)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefetch {
    pub memory: Option<MemoryPrefetch>,
    #[serde(default, skip_serializing_if = "is_unset_origin")]
    pub origin: Option<String>,
}

pub const PREFETCH_ORIGIN_BUILD: &str = "build";
pub const PREFETCH_ORIGIN_CHECKPOINT: &str = "checkpoint";
pub const PREFETCH_ORIGIN_HARVEST: &str = "harvest";

fn is_unset_origin(origin: &Option<String>) -> bool {
    origin.as_deref().is_none_or(str::is_empty)
}

impl Prefetch {
    pub fn init_origin(prefetch: Option<&Self>) -> &str {
        match prefetch {
            Some(Prefetch {
                memory: Some(memory),
                origin,
            }) if !memory.indices.is_empty() => origin.as_deref().filter(|origin| !origin.is_empty()).unwrap_or("unknown"),
            _ => "none",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPrefetch {
    #[serde(default, deserialize_with = "null_as_default")]
    pub indices: Vec<u64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub access_types: Vec<String>,
    pub block_size: i64,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

impl TemplateMetadata {
    pub fn v1() -> Self {
        Self {
            version: DEPRECATED_VERSION,
            ..Self::default()
        }
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|e| format!("error unmarshaling template version: {e}"))?;
        match value.get("version").and_then(Value::as_f64) {
            Some(version) if version > DEPRECATED_VERSION as f64 => {
                serde_json::from_value(value).map_err(|e| format!("error unmarshaling template metadata: {e}"))
            }
            _ => Ok(Self::v1()),
        }
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn based_on(&self, alias: &str, build_id: &str) -> Self {
        Self {
            version: CURRENT_VERSION,
            template: self.template.clone(),
            context: self.context.clone(),
            start: self.start.clone(),
            from_template: Some(FromTemplate {
                alias: alias.to_owned(),
                build_id: build_id.to_owned(),
            }),
            ..Self::default()
        }
    }

    pub fn new_version(&self, template: TemplateVersions) -> Self {
        Self {
            version: CURRENT_VERSION,
            template,
            context: self.context.clone(),
            start: self.start.clone(),
            from_image: self.from_image.clone(),
            from_template: self.from_template.clone(),
            cmdline_args: self.cmdline_args.clone(),
            balloon: self.balloon,
            cpu_template: self.cpu_template.clone(),
            build_cpu_template: self.build_cpu_template.clone(),
            vcpu_count: self.vcpu_count,
            ..Self::default()
        }
    }

    pub fn same_version(&self, template: TemplateVersions) -> Self {
        Self {
            template,
            prefetch: None,
            filesystem_only: false,
            fs_quiesced: false,
            ..self.clone()
        }
    }

    pub fn with_vcpu_count(&self, vcpus: i64) -> Self {
        Self {
            vcpu_count: vcpus,
            ..self.clone()
        }
    }

    pub fn with_prefetch(&self, memory: Option<MemoryPrefetch>, origin: &str) -> Self {
        Self {
            prefetch: Some(Prefetch {
                memory,
                origin: Some(origin).filter(|origin| !origin.is_empty()).map(str::to_owned),
            }),
            filesystem_only: false,
            fs_quiesced: false,
            ..self.clone()
        }
    }

    pub fn prefetch_init_origin(&self) -> &str {
        Prefetch::init_origin(self.prefetch.as_ref())
    }

    pub fn memory_prefetch(&self) -> Option<&MemoryPrefetch> {
        self.prefetch.as_ref().and_then(|prefetch| prefetch.memory.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn paused() -> TemplateMetadata {
        TemplateMetadata {
            version: CURRENT_VERSION,
            template: TemplateVersions {
                build_id: "b1".to_owned(),
                kernel_version: "vmlinux-6.1.102".to_owned(),
                firecracker_version: "v1.14.1".to_owned(),
            },
            context: Context {
                user: Some("user".to_owned()),
                workdir: Some("/home/user".to_owned()),
                env_vars: Some(BTreeMap::from([("A".to_owned(), "b".to_owned())])),
            },
            ..TemplateMetadata::default()
        }
    }

    #[test]
    fn serializes_with_upstream_keys_and_omits_empty_fields() {
        assert_eq!(
            String::from_utf8(paused().to_json()).unwrap(),
            r#"{"version":2,"template":{"build_id":"b1","kernel_version":"vmlinux-6.1.102","firecracker_version":"v1.14.1"},"context":{"user":"user","workdir":"/home/user","env_vars":{"A":"b"}}}"#
        );
        let empty = paused().with_prefetch(None, "");
        assert!(
            String::from_utf8(empty.to_json())
                .unwrap()
                .ends_with(r#""prefetch":{"memory":null}}"#)
        );
    }

    fn one_block() -> MemoryPrefetch {
        MemoryPrefetch {
            indices: vec![1],
            access_types: vec!["r".to_owned()],
            block_size: 4096,
        }
    }

    #[test]
    fn the_origin_rides_with_the_mapping_through_the_file() {
        let stamped = paused().with_prefetch(Some(one_block()), PREFETCH_ORIGIN_HARVEST);

        let json = stamped.to_json();
        let read = TemplateMetadata::from_json(&json).unwrap();

        assert!(String::from_utf8(json).unwrap().ends_with(r#""block_size":4096},"origin":"harvest"}}"#));
        assert_eq!(read.prefetch.as_ref().and_then(|prefetch| prefetch.origin.as_deref()), Some("harvest"));
        assert_eq!(read.prefetch_init_origin(), "harvest");
    }

    #[test]
    fn a_mapping_written_before_origins_existed_reads_back_as_unknown() {
        let legacy = paused().with_prefetch(Some(one_block()), "");

        let read = TemplateMetadata::from_json(&legacy.to_json()).unwrap();

        assert_eq!(read.prefetch.as_ref().and_then(|prefetch| prefetch.origin.as_deref()), None);
        assert_eq!(read.prefetch_init_origin(), "unknown");
        assert_eq!(Prefetch::init_origin(None), "none");
        assert_eq!(paused().with_prefetch(None, PREFETCH_ORIGIN_BUILD).prefetch_init_origin(), "none");
    }

    #[test]
    fn a_trace_round_trips() {
        let traced = paused().with_prefetch(
            Some(MemoryPrefetch {
                indices: vec![812, 3],
                access_types: vec!["r".to_owned(), "w".to_owned()],
                block_size: 4096,
            }),
            "",
        );

        let json = traced.to_json();

        assert!(
            String::from_utf8(json.clone())
                .unwrap()
                .contains(r#""prefetch":{"memory":{"indices":[812,3],"access_types":["r","w"],"block_size":4096}}"#)
        );
        assert_eq!(TemplateMetadata::from_json(&json).unwrap(), traced);
    }

    #[test]
    fn version_one_or_garbage_versions_read_as_v1() {
        let v1 = TemplateMetadata::v1();
        assert_eq!(
            String::from_utf8(v1.to_json()).unwrap(),
            r#"{"version":1,"template":{"build_id":"","kernel_version":"","firecracker_version":""},"context":{}}"#
        );
        assert_eq!(
            TemplateMetadata::from_json(br#"{"version":1,"context":{"user":"x"}}"#).unwrap(),
            v1
        );
        assert_eq!(TemplateMetadata::from_json(br#"{"version":"2"}"#).unwrap(), v1);
        assert!(TemplateMetadata::from_json(br#"{"version":2,"template":{"build_id":"b","kernel_version":"k","firecracker_version":"f"},"context":{},"unknown":1}"#).is_ok());
    }

    #[test]
    fn a_fractional_version_fails_to_decode_like_go() {
        let error = TemplateMetadata::from_json(br#"{"version":1.5}"#).unwrap_err();

        assert!(error.starts_with("error unmarshaling template metadata: "), "{error}");
        assert_eq!(
            TemplateMetadata::from_json(br#"{"version":1.0}"#).unwrap(),
            TemplateMetadata::v1()
        );
        assert_eq!(
            TemplateMetadata::from_json(br#"{"version":null}"#).unwrap(),
            TemplateMetadata::v1()
        );
        assert!(
            TemplateMetadata::from_json(b"{")
                .unwrap_err()
                .starts_with("error unmarshaling template version: ")
        );
    }

    #[test]
    fn nested_objects_keep_go_struct_order() {
        let based = paused().based_on("base", "b0");
        let json = String::from_utf8(based.to_json()).unwrap();

        assert!(
            json.ends_with(r#""from_template":{"alias":"base","build_id":"b0"}}"#),
            "{json}"
        );
        let cpu =
            br#"{"version":2,"cpu_template":{"x86_tsc_khz":3200000,"msr_modifiers":[{"bitmap":"0b0","addr":"0x1"}]}}"#;
        let json = String::from_utf8(TemplateMetadata::from_json(cpu).unwrap().to_json()).unwrap();
        assert!(
            json.ends_with(
                r#""cpu_template":{"msr_modifiers":[{"addr":"0x1","bitmap":"0b0"}],"x86_tsc_khz":3200000}}"#
            ),
            "{json}"
        );
    }

    #[test]
    fn a_paused_build_keeps_the_context_and_drops_the_trace() {
        let build = Uuid::from_bytes([7; 16]);
        let traced = paused().with_prefetch(Some(MemoryPrefetch::default()), "");

        let versions = TemplateVersions {
            build_id: build.to_string(),
            kernel_version: "vmlinux-6.1.158".to_owned(),
            firecracker_version: "v1.14".to_owned(),
        };
        let next = traced.same_version(versions.clone());

        assert_eq!(next.template, versions);
        assert_eq!((next.context, next.prefetch), (paused().context, None));
    }

    #[test]
    fn go_style_nulls_and_missing_objects_are_accepted() {
        let lenient = TemplateMetadata::from_json(
            br#"{"version":2,"context":{"env_vars":null},"cmdline_args":null,"prefetch":{"memory":{"indices":null,"access_types":null,"block_size":4096}}}"#,
        )
        .unwrap();

        assert_eq!(lenient.template, TemplateVersions::default());
        assert_eq!(lenient.context.env_vars, None);
        assert_eq!(lenient.memory_prefetch().map(|memory| memory.indices.len()), Some(0));
    }

    #[test]
    fn a_new_version_keeps_the_build_context_and_drops_the_rest() {
        let source = TemplateMetadata {
            version: 1,
            context: Context {
                user: Some("user".to_owned()),
                ..Context::default()
            },
            start: Some(Start {
                start_command: "x".to_owned(),
                ..Start::default()
            }),
            from_image: Some("debian".to_owned()),
            prefetch: Some(Prefetch {
                memory: None,
                origin: None,
            }),
            filesystem_only: true,
            fs_quiesced: true,
            ..TemplateMetadata::default()
        };
        let template = TemplateVersions {
            build_id: "b".to_owned(),
            kernel_version: "k".to_owned(),
            firecracker_version: "f".to_owned(),
        };

        let next = source.new_version(template.clone());

        assert_eq!(next.version, CURRENT_VERSION);
        assert_eq!(next.template, template);
        assert_eq!(
            (next.context, next.start, next.from_image),
            (source.context, source.start, source.from_image)
        );
        assert_eq!(
            (next.prefetch, next.filesystem_only, next.fs_quiesced),
            (None, false, false)
        );
    }

    #[test]
    fn a_partial_balloon_reads_like_go() {
        let metadata = TemplateMetadata::from_json(br#"{"version":2,"balloon":{"reporting":true}}"#).unwrap();

        assert_eq!(
            metadata.balloon,
            Some(Balloon {
                reporting: true,
                hinting: false
            })
        );
    }

    #[test]
    fn start_is_written_with_every_key_like_go() {
        let started = TemplateMetadata {
            version: CURRENT_VERSION,
            start: Some(Start {
                start_command: "npm start".to_owned(),
                ready_command: String::new(),
                context: Context {
                    user: Some("user".to_owned()),
                    ..Context::default()
                },
            }),
            ..TemplateMetadata::default()
        };

        let json = String::from_utf8(started.to_json()).unwrap();

        assert!(json.contains(r#""start":{"start_command":"npm start","ready_command":"","context":{"user":"user"}}"#));
        assert_eq!(TemplateMetadata::from_json(json.as_bytes()).unwrap(), started);
    }

    #[test]
    fn every_copy_constructor_carries_the_vcpu_count_but_a_from_template_build_does_not() {
        let base = TemplateMetadata {
            version: CURRENT_VERSION,
            template: TemplateVersions {
                build_id: "build-1".to_owned(),
                ..TemplateVersions::default()
            },
            vcpu_count: 16,
            ..TemplateMetadata::default()
        };
        let next = TemplateVersions {
            build_id: "build-2".to_owned(),
            ..TemplateVersions::default()
        };

        for (name, copy) in [
            ("same_version", base.same_version(next.clone())),
            ("new_version", base.new_version(next)),
            ("with_prefetch", base.with_prefetch(Some(MemoryPrefetch::default()), "")),
            (
                "with_vcpu_count",
                TemplateMetadata {
                    version: CURRENT_VERSION,
                    ..TemplateMetadata::default()
                }
                .with_vcpu_count(16),
            ),
        ] {
            assert_eq!(copy.vcpu_count, 16, "{name}");
        }
        assert_eq!(base.based_on("a", "build-0").vcpu_count, 0);
    }

    #[test]
    fn the_vcpu_count_is_written_after_the_cpu_templates_and_skipped_when_unrecorded() {
        let json = |count| String::from_utf8(paused().with_vcpu_count(count).to_json()).unwrap();

        assert!(json(4).ends_with(r#""vcpu_count":4}"#), "{}", json(4));
        assert!(!json(0).contains("vcpu_count"));
        assert_eq!(TemplateMetadata::from_json(json(4).as_bytes()).unwrap().vcpu_count, 4);
    }

    #[test]
    fn a_v1_lineage_keeps_its_version_and_loses_the_count() {
        let stamped = TemplateMetadata::v1().with_vcpu_count(4);
        assert_eq!(stamped.version, DEPRECATED_VERSION);
        assert_eq!(stamped.vcpu_count, 4);

        let decoded = TemplateMetadata::from_json(&stamped.to_json()).unwrap();

        assert_eq!(decoded.version, DEPRECATED_VERSION);
        assert_eq!(decoded.vcpu_count, 0);
    }
}
