use std::time::SystemTime;

use chrono::{DateTime, Utc};
use klickhouse::{DateTime64, Row, Uuid};

use crate::events::SandboxEvent;

pub const DEFAULT_EVENTS_TTL_DAYS: i64 = 7;
pub const MAX_EVENTS_TTL_DAYS: i64 = 365;
pub const INSERT_EVENTS: &str = "INSERT INTO sandbox_events (timestamp, sandbox_id, sandbox_execution_id, sandbox_template_id, sandbox_build_id, sandbox_team_id, event_data, type, version, id, events_ttl_days) SETTINGS wait_for_async_insert=1 FORMAT Native";
pub const INSERT_HOST_STATS: &str = "INSERT INTO sandbox_host_stats (timestamp, sandbox_id, sandbox_execution_id, sandbox_template_id, sandbox_build_id, sandbox_team_id, sandbox_vcpu_count, sandbox_memory_mb, cgroup_cpu_usage_usec, cgroup_cpu_user_usec, cgroup_cpu_system_usec, cgroup_memory_usage_bytes, cgroup_memory_peak_bytes, delta_cgroup_cpu_usage_usec, delta_cgroup_cpu_user_usec, delta_cgroup_cpu_system_usec, interval_us, sandbox_type) SETTINGS wait_for_async_insert=1 FORMAT Native";

#[derive(Row, Debug, Clone)]
pub struct EventRow {
    pub timestamp: DateTime64<9>,
    pub sandbox_id: String,
    pub sandbox_execution_id: String,
    pub sandbox_template_id: String,
    pub sandbox_build_id: String,
    pub sandbox_team_id: Uuid,
    pub event_data: Option<String>,
    #[klickhouse(rename = "type")]
    pub kind: String,
    pub version: String,
    pub id: Uuid,
    pub events_ttl_days: i64,
}

#[derive(Row, Debug, Clone, Default)]
pub struct HostStatRow {
    pub timestamp: DateTime64<9>,
    pub sandbox_id: String,
    pub sandbox_execution_id: String,
    pub sandbox_template_id: String,
    pub sandbox_build_id: String,
    pub sandbox_team_id: Uuid,
    pub sandbox_vcpu_count: i64,
    pub sandbox_memory_mb: i64,
    pub cgroup_cpu_usage_usec: u64,
    pub cgroup_cpu_user_usec: u64,
    pub cgroup_cpu_system_usec: u64,
    pub cgroup_memory_usage_bytes: u64,
    pub cgroup_memory_peak_bytes: u64,
    pub delta_cgroup_cpu_usage_usec: u64,
    pub delta_cgroup_cpu_user_usec: u64,
    pub delta_cgroup_cpu_system_usec: u64,
    pub interval_us: u64,
    pub sandbox_type: String,
}

pub fn timestamp(time: SystemTime) -> DateTime64<9> {
    DateTime::<Utc>::from(time).try_into().unwrap_or_default()
}

impl EventRow {
    pub fn from_event(event: &SandboxEvent) -> Self {
        Self {
            timestamp: timestamp(event.at),
            sandbox_id: event.sandbox_id.clone(),
            sandbox_execution_id: event.sandbox_execution_id.clone(),
            sandbox_template_id: event.sandbox_template_id.clone(),
            sandbox_build_id: event.sandbox_build_id.clone(),
            sandbox_team_id: event.sandbox_team_id,
            event_data: serde_json::to_string(&event.event_data)
                .ok()
                .filter(|data| !data.is_empty()),
            kind: event.kind.clone(),
            version: event.version.clone(),
            id: event.id,
            events_ttl_days: match event.events_ttl_days {
                days if days <= 0 => DEFAULT_EVENTS_TTL_DAYS,
                days => days.min(MAX_EVENTS_TTL_DAYS),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::events::{EventSandbox, KILLED};

    fn event(ttl: i64) -> SandboxEvent {
        SandboxEvent::new(
            KILLED,
            &EventSandbox {
                sandbox_id: "sbx".to_owned(),
                execution_id: "exec".to_owned(),
                base_template_id: "base".to_owned(),
                build_id: "build".to_owned(),
                team_id: Uuid::nil().to_string(),
                events_ttl_days: ttl,
                metadata: BTreeMap::new(),
            },
        )
    }

    #[test]
    fn ttl_days_default_to_7_and_cap_at_365() {
        assert_eq!(EventRow::from_event(&event(0)).events_ttl_days, 7);
        assert_eq!(EventRow::from_event(&event(30)).events_ttl_days, 30);
        assert_eq!(EventRow::from_event(&event(1000)).events_ttl_days, 365);
    }

    #[test]
    fn event_data_is_stored_as_json() {
        let row = EventRow::from_event(&event(0).with("kill_reason", serde_json::json!("unknown")));

        assert_eq!(row.event_data.as_deref(), Some(r#"{"kill_reason":"unknown"}"#));
        assert_eq!(
            (row.kind.as_str(), row.version.as_str()),
            ("sandbox.lifecycle.killed", "v2")
        );
    }
}
