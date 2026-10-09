use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use super::{counter, milliseconds};

static ATTEMPTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.envd.upgrade.attempts",
        "Resume-time envd live-upgrade attempts, by result and from/to version",
        "{attempt}",
    )
});

static GATED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.envd.upgrade.gated",
        "Resumes where the envd-upgrade-target flag named a target but a gate declined the upgrade, by reason",
        "{sandbox}",
    )
});

static HANDOVER: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.envd.upgrade.handover",
        "Live-upgrade handover items by item (proc|retained|watcher) and result (ok|failed)",
        "{item}",
    )
});

static DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.envd.upgrade.duration",
        "Wall-time of a resume-time envd upgrade (delivery + trigger + WaitForEnvd)",
    )
});

static PHASE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.envd.upgrade.phase.duration",
        "Wall-time of one resume-time envd upgrade phase (resolve|deliver|ready); resolve is absent from the combined histogram",
    )
});

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeAttempt {
    pub result: String,
    pub from_version: String,
    pub to_version: String,
}

impl UpgradeAttempt {
    pub fn record(&self, took: Duration) {
        ATTEMPTS.add(
            1,
            &[
                KeyValue::new("result", self.result.clone()),
                KeyValue::new("from_version", self.from_version.clone()),
                KeyValue::new("to_version", self.to_version.clone()),
            ],
        );
        DURATION.record(millis(took), &[KeyValue::new("result", self.result.clone())]);
    }
}

pub fn record_phase(phase: &str, result: &str, took: Duration) {
    PHASE_DURATION.record(
        millis(took),
        &[KeyValue::new("phase", phase.to_owned()), KeyValue::new("result", result.to_owned())],
    );
}

pub fn record_gated(reason: &str) {
    GATED.add(1, &[KeyValue::new("reason", reason.to_owned())]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandoverItem {
    pub name: &'static str,
    pub total: u64,
    pub failed: u64,
}

impl HandoverItem {
    pub fn record(&self) {
        self.add(self.total.saturating_sub(self.failed), "ok");
        self.add(self.failed, "failed");
    }

    fn add(&self, count: u64, result: &'static str) {
        HANDOVER.add(
            count,
            &[KeyValue::new("item", self.name), KeyValue::new("result", result)],
        );
    }
}
