use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use crate::go_time::{rfc3339, rfc3339_nano};
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub const VERSION: &str = "v2";
pub const CREATED: &str = "sandbox.lifecycle.created";
pub const KILLED: &str = "sandbox.lifecycle.killed";
pub const PAUSED: &str = "sandbox.lifecycle.paused";
pub const RESUMED: &str = "sandbox.lifecycle.resumed";
pub const UPDATED: &str = "sandbox.lifecycle.updated";
pub const CHECKPOINTED: &str = "sandbox.lifecycle.checkpointed";
pub const STREAM: &str = "sandbox.events.stream";

const PAUSE_MODE_FULL: &str = "full";
const PAUSE_MODE_FILESYSTEM: &str = "filesystem";
const RESUME_MODE_RESTORE: &str = "restore";
const RESUME_MODE_REBOOT: &str = "reboot";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SandboxEvent {
    pub id: Uuid,
    pub version: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: String,
    #[serde(skip)]
    pub at: SystemTime,
    pub event_category: String,
    pub event_label: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub event_data: BTreeMap<String, Value>,
    pub sandbox_id: String,
    pub sandbox_execution_id: String,
    pub sandbox_template_id: String,
    pub sandbox_build_id: String,
    pub sandbox_team_id: Uuid,
    #[serde(skip_serializing_if = "is_zero")]
    pub events_ttl_days: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSandbox {
    pub sandbox_id: String,
    pub execution_id: String,
    pub base_template_id: String,
    pub build_id: String,
    pub team_id: String,
    pub events_ttl_days: i64,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Execution {
    pub started_at: SystemTime,
    pub vcpu_count: i64,
    pub memory_mb: i64,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

impl SandboxEvent {
    pub fn new(kind: &str, sandbox: &EventSandbox) -> Self {
        let mut event_data = BTreeMap::new();
        if !sandbox.metadata.is_empty() {
            event_data.insert("sandbox_metadata".to_owned(), json!(sandbox.metadata));
        }
        let at = SystemTime::now();
        Self {
            id: Uuid::new_v4(),
            version: VERSION.to_owned(),
            kind: kind.to_owned(),
            timestamp: rfc3339_nano(at),
            at,
            event_category: String::new(),
            event_label: String::new(),
            event_data,
            sandbox_id: sandbox.sandbox_id.clone(),
            sandbox_execution_id: sandbox.execution_id.clone(),
            sandbox_template_id: sandbox.base_template_id.clone(),
            sandbox_build_id: sandbox.build_id.clone(),
            sandbox_team_id: Uuid::parse_str(&sandbox.team_id).unwrap_or_default(),
            events_ttl_days: sandbox.events_ttl_days,
        }
    }

    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.event_data.insert(key.to_owned(), value);
        self
    }

    pub fn with_pause_mode(self, filesystem_only: bool) -> Self {
        let mode = match filesystem_only {
            true => PAUSE_MODE_FILESYSTEM,
            false => PAUSE_MODE_FULL,
        };
        self.with("pause_mode", json!(mode))
    }

    pub fn with_resume_mode(self, filesystem_booted: bool) -> Self {
        let mode = match filesystem_booted {
            true => RESUME_MODE_REBOOT,
            false => RESUME_MODE_RESTORE,
        };
        self.with("resume_mode", json!(mode))
    }

    pub fn with_execution(self, execution: &Execution) -> Self {
        let elapsed = SystemTime::now()
            .duration_since(execution.started_at)
            .unwrap_or_default()
            .as_millis() as u64;
        self.with(
            "execution",
            json!({
                "started_at": rfc3339(execution.started_at),
                "vcpu_count": execution.vcpu_count,
                "memory_mb": execution.memory_mb,
                "execution_time": elapsed,
            }),
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        let missing = [
            (self.version.is_empty(), "version"),
            (self.kind.is_empty(), "type"),
            (self.sandbox_id.is_empty(), "sandbox_id"),
            (self.sandbox_team_id.is_nil(), "sandbox_team_id"),
            (self.timestamp.is_empty(), "timestamp"),
        ]
        .into_iter()
        .find_map(|(missing, field)| missing.then_some(field));
        missing.map_or(Ok(()), |field| Err(format!("missing required event field: {field}")))
    }
}

pub fn delivery_key(team_id: Uuid) -> String {
    format!("wh:{team_id}")
}

pub trait EventSink: Send + Sync {
    fn publish(&self, event: &SandboxEvent) -> BoxFuture<'static, Result<(), String>>;
}

#[derive(Default)]
pub struct EventsService {
    sinks: OnceLock<Vec<Arc<dyn EventSink>>>,
}

impl EventsService {
    pub fn install(&self, sinks: Vec<Arc<dyn EventSink>>) {
        let _ = self.sinks.set(sinks);
    }

    pub fn publish(&self, event: SandboxEvent) {
        if let Err(e) = event.validate() {
            tracing::error!(error = %e, ?event, "Failed to publish sandbox event due to validation error");
            return;
        }
        let Some(sinks) = self.sinks.get() else {
            return;
        };
        for sink in sinks {
            let published = sink.publish(&event);
            tokio::spawn(async move {
                if let Err(e) = published.await {
                    tracing::error!(error = %e, "Failed to publish sandbox event");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(team: &str) -> EventSandbox {
        EventSandbox {
            sandbox_id: "sbx".to_owned(),
            execution_id: "exec".to_owned(),
            base_template_id: "base".to_owned(),
            build_id: "build".to_owned(),
            team_id: team.to_owned(),
            events_ttl_days: 0,
            metadata: BTreeMap::from([("k".to_owned(), "v".to_owned())]),
        }
    }

    #[test]
    fn events_serialize_with_upstream_fields() {
        let team = Uuid::from_bytes([1; 16]);
        let event = SandboxEvent::new(KILLED, &sandbox(&team.to_string())).with("kill_reason", json!("unknown"));

        let value = serde_json::to_value(&event).unwrap();

        assert_eq!(value["version"], "v2");
        assert_eq!(value["type"], "sandbox.lifecycle.killed");
        assert_eq!(
            (value["event_category"].as_str(), value["event_label"].as_str()),
            (Some(""), Some(""))
        );
        assert_eq!(
            value["event_data"],
            json!({"kill_reason": "unknown", "sandbox_metadata": {"k": "v"}})
        );
        assert_eq!(value["sandbox_template_id"], "base");
        assert_eq!(value["sandbox_team_id"], team.to_string());
        assert!(value.get("events_ttl_days").is_none());
        assert_eq!(delivery_key(team), format!("wh:{team}"));
    }

    #[test]
    fn an_event_without_a_team_is_invalid() {
        assert_eq!(
            SandboxEvent::new(CREATED, &sandbox("not-a-uuid")).validate(),
            Err("missing required event field: sandbox_team_id".to_owned())
        );
    }

    #[test]
    fn execution_details_are_attached() {
        let execution = Execution {
            started_at: SystemTime::UNIX_EPOCH,
            vcpu_count: 2,
            memory_mb: 1024,
        };

        let event = SandboxEvent::new(PAUSED, &sandbox("x")).with_execution(&execution);

        let data = &event.event_data["execution"];
        assert_eq!(
            (data["vcpu_count"].as_i64(), data["memory_mb"].as_i64()),
            (Some(2), Some(1024))
        );
        assert_eq!(data["started_at"], "1970-01-01T00:00:00Z");
    }

    #[test]
    fn the_pause_mode_names_what_the_snapshot_holds() {
        for (filesystem_only, want) in [(false, "full"), (true, "filesystem")] {
            let event = SandboxEvent::new(PAUSED, &sandbox("x"))
                .with("execution", json!({}))
                .with_pause_mode(filesystem_only);

            assert_eq!(event.event_data["pause_mode"], want);
            assert!(event.event_data.contains_key("execution"), "existing keys are kept");
        }
    }

    #[test]
    fn the_resume_mode_names_the_boot_path_that_ran() {
        for (filesystem_booted, want) in [(false, "restore"), (true, "reboot")] {
            let event = SandboxEvent::new(RESUMED, &sandbox("x")).with_resume_mode(filesystem_booted);

            assert_eq!(event.event_data["resume_mode"], want);
        }
    }
}
