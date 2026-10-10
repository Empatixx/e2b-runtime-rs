use serde::Deserialize;

use crate::envd::version::is_at_least;
use crate::oom::OomKill;

pub const MIN_ENVD_FOR_METRICS: &str = "0.1.5";
pub const MIN_ENVD_FOR_METRICS_TIMESTAMP: &str = "0.1.3";
pub const MIN_ENVD_FOR_MEMORY_METRICS: &str = "0.2.4";
pub const MIN_ENVD_FOR_DISK_METRICS: &str = "0.2.4";
pub const MIN_ENVD_FOR_CACHE_METRICS: &str = "0.5.9";

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SandboxMetrics {
    #[serde(rename = "ts")]
    pub timestamp: i64,
    pub cpu_count: i64,
    #[serde(rename = "cpu_used_pct")]
    pub cpu_used_percent: f64,
    pub cpu_possible: i64,
    pub cpu_target: i64,
    pub cpu_target_attempts: i64,
    pub cpu_write_pending_ms: i64,
    pub mem_total: i64,
    pub mem_used: i64,
    pub mem_cache: i64,
    pub disk_used: i64,
    pub disk_total: i64,
    pub oom_kills: Option<Vec<OomKill>>,
}

impl SandboxMetrics {
    pub fn reports_cpu_hotplug(&self) -> bool {
        self.cpu_possible > 0 || self.cpu_target > 0
    }
}

pub fn sandbox_memory(envd_version: &str, metrics: &SandboxMetrics) -> Result<Option<(i64, i64)>, String> {
    is_at_least(envd_version, MIN_ENVD_FOR_MEMORY_METRICS)
        .map(|reported| reported.then_some((metrics.mem_total, metrics.mem_used)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_carry_cpu_hotplug() {
        let metrics: SandboxMetrics = serde_json::from_str(
            r#"{"cpu_count":2,"cpu_possible":16,"cpu_target":2,"cpu_target_attempts":1,"cpu_write_pending_ms":0}"#,
        )
        .unwrap();

        assert_eq!(
            metrics,
            SandboxMetrics {
                cpu_count: 2,
                cpu_possible: 16,
                cpu_target: 2,
                cpu_target_attempts: 1,
                ..SandboxMetrics::default()
            }
        );
        assert!(metrics.reports_cpu_hotplug());
    }

    #[test]
    fn an_envd_without_hotplug_reports_neither_possible_nor_target() {
        let old: SandboxMetrics = serde_json::from_str(r#"{"cpu_count":2}"#).unwrap();

        assert_eq!((old.cpu_possible, old.cpu_target), (0, 0));
        assert!(!old.reports_cpu_hotplug());
    }

    #[test]
    fn a_failed_sysfs_read_still_reports_the_target() {
        let metrics: SandboxMetrics = serde_json::from_str(r#"{"cpu_count":2,"cpu_target":2}"#).unwrap();

        assert!(metrics.reports_cpu_hotplug());
    }

    #[test]
    fn memory_is_reported_in_bytes_from_envd_0_2_4() {
        let metrics = SandboxMetrics {
            mem_total: 4 << 30,
            mem_used: 1 << 30,
            ..SandboxMetrics::default()
        };

        for (name, version, want) in [
            ("first envd reporting bytes", "0.2.4", Some(Some((4 << 30, 1 << 30)))),
            ("current envd", "0.9.0", Some(Some((4 << 30, 1 << 30)))),
            ("envd reporting only MiB", "0.2.3", Some(None)),
            ("unparsable version", "not-a-version", None),
        ] {
            assert_eq!(sandbox_memory(version, &metrics).ok(), want, "{name}");
        }
    }
}
