use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub start: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_template: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetch: Option<Prefetch>,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub cmdline_args: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balloon: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_template: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_cpu_template: Option<Value>,
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
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    #[serde(default, skip_serializing_if = "is_nil_or_empty")]
    pub env_vars: Option<BTreeMap<String, String>>,
}

fn is_nil_or_empty(env_vars: &Option<BTreeMap<String, String>>) -> bool {
    env_vars.as_ref().is_none_or(BTreeMap::is_empty)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefetch {
    pub memory: Option<MemoryPrefetch>,
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
        let value: Value = serde_json::from_slice(bytes).map_err(|e| format!("decode metadata.json: {e}"))?;
        match value.get("version").and_then(Value::as_u64) {
            Some(version) if version > DEPRECATED_VERSION => {
                serde_json::from_value(value).map_err(|e| format!("decode metadata.json: {e}"))
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
            from_template: Some(serde_json::json!({ "alias": alias, "build_id": build_id })),
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
            balloon: self.balloon.clone(),
            cpu_template: self.cpu_template.clone(),
            build_cpu_template: self.build_cpu_template.clone(),
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

    pub fn with_prefetch(&self, memory: Option<MemoryPrefetch>) -> Self {
        Self {
            prefetch: Some(Prefetch { memory }),
            filesystem_only: false,
            fs_quiesced: false,
            ..self.clone()
        }
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
        let empty = paused().with_prefetch(None);
        assert!(
            String::from_utf8(empty.to_json())
                .unwrap()
                .ends_with(r#""prefetch":{"memory":null}}"#)
        );
    }

    #[test]
    fn a_trace_round_trips() {
        let traced = paused().with_prefetch(Some(MemoryPrefetch {
            indices: vec![812, 3],
            access_types: vec!["r".to_owned(), "w".to_owned()],
            block_size: 4096,
        }));

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
    fn a_paused_build_keeps_the_context_and_drops_the_trace() {
        let build = Uuid::from_bytes([7; 16]);
        let traced = paused().with_prefetch(Some(MemoryPrefetch::default()));

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
            start: Some(serde_json::json!({ "start_command": "x" })),
            from_image: Some("debian".to_owned()),
            prefetch: Some(Prefetch { memory: None }),
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
}
