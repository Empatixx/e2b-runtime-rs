use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

const CLOCK_SKEW_TOLERANCE: Duration = Duration::from_secs(60);
const UNTRUSTED_FIELDS: [&str; 17] = [
    "instanceID",
    "sandboxID",
    "sandbox_id",
    "sandbox.id",
    "envID",
    "env_id",
    "env.id",
    "templateID",
    "template_id",
    "template.id",
    "teamID",
    "team_id",
    "team.id",
    "buildID",
    "build_id",
    "build.id",
    "internal",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSandbox {
    pub sandbox_id: String,
    pub template_id: String,
    pub team_id: String,
    pub build_id: String,
    pub lifecycle_started_at: Option<SystemTime>,
}

pub fn validate_sandbox_id(payload: &Map<String, Value>, sandbox_id: &str) -> Result<(), String> {
    match payload.get("instanceID") {
        None | Some(Value::Null) => Err("missing sandboxID in logs payload".to_owned()),
        Some(Value::String(claimed)) if claimed == sandbox_id => Ok(()),
        Some(Value::String(claimed)) => Err(format!(
            "sandboxID in logs payload does not match the sandboxID of the source sandbox ({claimed} != {sandbox_id})"
        )),
        Some(other) => Err(format!("instanceID in logs payload is not a string: {other}")),
    }
}

pub fn has_stale_timestamp(payload: &Map<String, Value>, lifecycle_started_at: Option<SystemTime>) -> bool {
    let Some(started) = lifecycle_started_at else {
        return false;
    };
    payload
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|timestamp| SystemTime::from(timestamp.with_timezone(&Utc)))
        .is_some_and(|timestamp| timestamp < started - CLOCK_SKEW_TOLERANCE)
}

pub fn set_trusted_identity(payload: &mut Map<String, Value>, source: &SourceSandbox) {
    UNTRUSTED_FIELDS.iter().for_each(|field| {
        payload.remove(*field);
    });
    payload.insert("instanceID".to_owned(), Value::from(source.sandbox_id.as_str()));
    payload.insert("envID".to_owned(), Value::from(source.template_id.as_str()));
    payload.insert("teamID".to_owned(), Value::from(source.team_id.as_str()));
    if !source.build_id.is_empty() {
        payload.insert("buildID".to_owned(), Value::from(source.build_id.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn source() -> SourceSandbox {
        SourceSandbox {
            sandbox_id: "sandbox-1".to_owned(),
            template_id: "template-1".to_owned(),
            team_id: "team-1".to_owned(),
            build_id: String::new(),
            lifecycle_started_at: Some(SystemTime::now() - Duration::from_secs(60)),
        }
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn the_payload_sandbox_id_must_match_the_source() {
        assert_eq!(
            validate_sandbox_id(&object(json!({})), "sandbox-1"),
            Err("missing sandboxID in logs payload".to_owned())
        );
        assert_eq!(
            validate_sandbox_id(&object(json!({"instanceID": "other"})), "sandbox-1"),
            Err(
                "sandboxID in logs payload does not match the sandboxID of the source sandbox (other != sandbox-1)"
                    .to_owned()
            )
        );
        assert_eq!(
            validate_sandbox_id(&object(json!({"instanceID": 7})), "sandbox-1"),
            Err("instanceID in logs payload is not a string: 7".to_owned())
        );
        assert_eq!(
            validate_sandbox_id(&object(json!({"instanceID": "sandbox-1"})), "sandbox-1"),
            Ok(())
        );
    }

    #[test]
    fn untrusted_identity_is_replaced_and_an_empty_build_is_omitted() {
        let mut payload = object(json!({
            "instanceID": "sandbox-1", "sandbox_id": "x", "template.id": "x", "team_id": "x",
            "buildID": "x", "internal": true, "message": "hello"
        }));

        set_trusted_identity(&mut payload, &source());

        assert_eq!(
            Value::Object(payload),
            json!({"instanceID": "sandbox-1", "envID": "template-1", "teamID": "team-1", "message": "hello"})
        );
    }

    #[test]
    fn logs_from_before_the_resume_are_stale() {
        let started = SystemTime::now();
        let old = DateTime::<Utc>::from(started - Duration::from_secs(120)).to_rfc3339();
        let recent = DateTime::<Utc>::from(started - Duration::from_secs(30)).to_rfc3339();

        assert!(has_stale_timestamp(&object(json!({ "timestamp": old })), Some(started)));
        assert!(!has_stale_timestamp(
            &object(json!({ "timestamp": recent })),
            Some(started)
        ));
        assert!(!has_stale_timestamp(
            &object(json!({ "timestamp": "garbage" })),
            Some(started)
        ));
        assert!(!has_stale_timestamp(&object(json!({ "timestamp": old })), None));
    }
}
