use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde_json::value::RawValue;

use crate::host::nfs::VolumeMount;

#[derive(Debug, Default, PartialEq)]
pub struct InitRequest {
    pub env_vars: Option<BTreeMap<String, String>>,
    pub access_token: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub default_user: Option<String>,
    pub default_workdir: Option<String>,
    pub hyperloop_ip: Option<String>,
    pub ca_bundle: Option<String>,
    pub volume_mounts: Option<Vec<VolumeMount>>,
    pub lifecycle_id: Option<String>,
}

struct Fields {
    entries: Vec<(String, Box<RawValue>)>,
}

impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(FieldsVisitor)
    }
}

struct FieldsVisitor;

impl<'de> Visitor<'de> for FieldsVisitor {
    type Value = Fields;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = map.next_entry::<String, Box<RawValue>>()? {
            entries.push(entry);
        }
        Ok(Fields { entries })
    }
}

pub fn parse(body: &[u8]) -> Result<InitRequest, String> {
    let fields: Fields = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    fields
        .entries
        .into_iter()
        .try_fold(InitRequest::default(), |request, (key, value)| {
            assign(request, &key, &value)
        })
}

fn assign(mut request: InitRequest, key: &str, value: &RawValue) -> Result<InitRequest, String> {
    let named = |name: &str| key.eq_ignore_ascii_case(name);
    match () {
        () if named("envVars") => request.env_vars = optional(value)?,
        () if named("accessToken") => request.access_token = secure_token(value.get())?,
        () if named("timestamp") => request.timestamp = optional(value)?,
        () if named("defaultUser") => request.default_user = optional(value)?,
        () if named("defaultWorkdir") => request.default_workdir = optional(value)?,
        () if named("hyperloopIP") => request.hyperloop_ip = optional(value)?,
        () if named("caBundle") => request.ca_bundle = optional(value)?,
        () if named("lifecycleID") => request.lifecycle_id = optional(value)?,
        () if named("volumeMounts") => request.volume_mounts = volume_mounts(value)?,
        () => {}
    }
    Ok(request)
}

fn optional<T: DeserializeOwned>(value: &RawValue) -> Result<Option<T>, String> {
    serde_json::from_str(value.get()).map_err(|e| e.to_string())
}

fn secure_token(raw: &str) -> Result<Option<String>, String> {
    if raw == "null" {
        return Ok(None);
    }
    let content = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .ok_or("invalid secure token JSON string")?;
    if content.contains('\\') {
        return Err("invalid secure token: unexpected escape sequence".to_owned());
    }
    if content.is_empty() {
        return Err("empty token not allowed".to_owned());
    }
    Ok(Some(content.to_owned()))
}

fn volume_mounts(value: &RawValue) -> Result<Option<Vec<VolumeMount>>, String> {
    let mounts: Option<Vec<Fields>> = optional(value)?;
    mounts
        .map(|mounts| {
            mounts
                .into_iter()
                .map(|mount| {
                    mount
                        .entries
                        .into_iter()
                        .try_fold(VolumeMount::default(), |mut volume, (key, value)| {
                            match () {
                                () if key.eq_ignore_ascii_case("nfs_target") => {
                                    volume.nfs_target = serde_json::from_str(value.get()).map_err(|e| e.to_string())?
                                }
                                () if key.eq_ignore_ascii_case("path") => {
                                    volume.path = serde_json::from_str(value.get()).map_err(|e| e.to_string())?
                                }
                                () => {}
                            }
                            Ok(volume)
                        })
                })
                .collect()
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_case_insensitively_and_the_last_one_wins() {
        let request = parse(br#"{"ACCESSTOKEN":"a","accessToken":"b","defaultuser":"user","unknown":1}"#).unwrap();
        assert_eq!(request.access_token.as_deref(), Some("b"));
        assert_eq!(request.default_user.as_deref(), Some("user"));
    }

    #[test]
    fn tokens_follow_secure_token_rules() {
        assert_eq!(parse(br#"{"accessToken":null}"#).unwrap().access_token, None);
        assert_eq!(parse(br#"{"accessToken":""}"#).unwrap_err(), "empty token not allowed");
        assert_eq!(
            parse(br#"{"accessToken":"a\u0041"}"#).unwrap_err(),
            "invalid secure token: unexpected escape sequence"
        );
        assert_eq!(
            parse(br#"{"accessToken":5}"#).unwrap_err(),
            "invalid secure token JSON string"
        );
    }

    #[test]
    fn volume_mounts_are_read_with_their_wire_keys() {
        let request =
            parse(br#"{"volumeMounts":[{"NFS_TARGET":"10.0.0.1:/v","path":"/mnt/v"}],"lifecycleID":"l1"}"#).unwrap();
        assert_eq!(
            request.volume_mounts,
            Some(vec![VolumeMount {
                nfs_target: "10.0.0.1:/v".to_owned(),
                path: "/mnt/v".to_owned(),
            }])
        );
        assert_eq!(request.lifecycle_id.as_deref(), Some("l1"));
        assert!(parse(br#"{"volumeMounts":"x"}"#).is_err());
    }
}
