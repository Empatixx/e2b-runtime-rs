use std::time::SystemTime;

use klickhouse::Uuid;

use crate::clickhouse::rows::{HostStatRow, timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CgroupSample {
    pub cpu_usage_usec: u64,
    pub cpu_user_usec: u64,
    pub cpu_system_usec: u64,
    pub memory_usage_bytes: u64,
    pub memory_peak_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsIdentity {
    pub sandbox_id: String,
    pub execution_id: String,
    pub template_id: String,
    pub build_id: String,
    pub team_id: String,
    pub vcpu_count: i64,
    pub memory_mb: i64,
}

pub struct HostStats {
    identity: StatsIdentity,
    team_id: Uuid,
    previous: CgroupSample,
    previous_at: SystemTime,
}

impl HostStats {
    pub fn start(identity: StatsIdentity, now: SystemTime) -> (Self, HostStatRow) {
        let team_id = Uuid::parse_str(&identity.team_id).unwrap_or_else(|_| {
            tracing::warn!(team_id = %identity.team_id, "invalid team ID for host stats, using zero UUID");
            Uuid::nil()
        });
        let stats = Self {
            identity,
            team_id,
            previous: CgroupSample::default(),
            previous_at: now,
        };
        let baseline = stats.row(CgroupSample::default(), now, CgroupSample::default(), 0);
        (stats, baseline)
    }

    pub fn sample(&self, current: CgroupSample, now: SystemTime) -> HostStatRow {
        let delta = CgroupSample {
            cpu_usage_usec: current.cpu_usage_usec.saturating_sub(self.previous.cpu_usage_usec),
            cpu_user_usec: current.cpu_user_usec.saturating_sub(self.previous.cpu_user_usec),
            cpu_system_usec: current.cpu_system_usec.saturating_sub(self.previous.cpu_system_usec),
            ..CgroupSample::default()
        };
        let interval = now.duration_since(self.previous_at).unwrap_or_default().as_micros() as u64;
        self.row(current, now, delta, interval)
    }

    pub fn advance(&mut self, current: CgroupSample, now: SystemTime) {
        self.previous = current;
        self.previous_at = now;
    }

    fn row(&self, current: CgroupSample, now: SystemTime, delta: CgroupSample, interval_us: u64) -> HostStatRow {
        HostStatRow {
            timestamp: timestamp(now),
            sandbox_id: self.identity.sandbox_id.clone(),
            sandbox_execution_id: self.identity.execution_id.clone(),
            sandbox_template_id: self.identity.template_id.clone(),
            sandbox_build_id: self.identity.build_id.clone(),
            sandbox_team_id: self.team_id,
            sandbox_vcpu_count: self.identity.vcpu_count,
            sandbox_memory_mb: self.identity.memory_mb,
            cgroup_cpu_usage_usec: current.cpu_usage_usec,
            cgroup_cpu_user_usec: current.cpu_user_usec,
            cgroup_cpu_system_usec: current.cpu_system_usec,
            cgroup_memory_usage_bytes: current.memory_usage_bytes,
            cgroup_memory_peak_bytes: current.memory_peak_bytes,
            delta_cgroup_cpu_usage_usec: delta.cpu_usage_usec,
            delta_cgroup_cpu_user_usec: delta.cpu_user_usec,
            delta_cgroup_cpu_system_usec: delta.cpu_system_usec,
            interval_us,
            sandbox_type: "sandbox".to_owned(),
        }
    }
}

pub fn parse_cpu_stat(text: &str) -> (u64, u64, u64) {
    let field = |name: &str| {
        text.lines()
            .filter_map(|line| line.split_once(' '))
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0)
    };
    (field("usage_usec"), field("user_usec"), field("system_usec"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn identity() -> StatsIdentity {
        StatsIdentity {
            sandbox_id: "sbx".to_owned(),
            execution_id: "exec".to_owned(),
            template_id: "tpl".to_owned(),
            build_id: "build".to_owned(),
            team_id: "not-a-uuid".to_owned(),
            vcpu_count: 2,
            memory_mb: 1024,
        }
    }

    #[test]
    fn the_baseline_is_zero_and_samples_carry_saturating_deltas() {
        let start = SystemTime::UNIX_EPOCH;
        let (mut stats, baseline) = HostStats::start(identity(), start);
        assert_eq!((baseline.cgroup_cpu_usage_usec, baseline.interval_us), (0, 0));
        assert_eq!(baseline.sandbox_team_id, Uuid::nil());

        let first = CgroupSample {
            cpu_usage_usec: 500,
            cpu_user_usec: 300,
            cpu_system_usec: 200,
            memory_usage_bytes: 10,
            memory_peak_bytes: 20,
        };
        let row = stats.sample(first, start + Duration::from_secs(1));
        stats.advance(first, start + Duration::from_secs(1));
        assert_eq!((row.delta_cgroup_cpu_usage_usec, row.interval_us), (500, 1_000_000));

        let reset = CgroupSample {
            cpu_usage_usec: 100,
            ..first
        };
        let after_reset = stats.sample(reset, start + Duration::from_secs(2));
        assert_eq!(after_reset.delta_cgroup_cpu_usage_usec, 0);
        assert_eq!(after_reset.sandbox_type, "sandbox");
    }

    #[test]
    fn cpu_stat_is_parsed() {
        assert_eq!(
            parse_cpu_stat("usage_usec 1200\nuser_usec 700\nsystem_usec 500\nnr_periods 0\n"),
            (1200, 700, 500)
        );
    }
}
