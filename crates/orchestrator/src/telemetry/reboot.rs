use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static FS_RECOVERY_RUNS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.fs_recovery.runs")
        .with_description("Pre-boot filesystem-recovery decisions on cold boots, by result and trigger")
        .with_unit("{run}")
        .build()
});

static FS_RECOVERY_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.fs_recovery.duration")
        .with_description("Wall-time of the jailed pre-boot e2fsck run on a cold boot")
        .with_unit("ms")
        .build()
});

static FS_RECOVERY_TOOLING_UNSUPPORTED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.fs_recovery.tooling_unsupported")
        .with_description("Fires once per process when the host e2fsck rejects -E journal_only")
        .with_unit("{probe}")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsRecoveryRun {
    pub result: &'static str,
    pub reason: &'static str,
    pub trigger: &'static str,
}

impl FsRecoveryRun {
    fn attributes(self) -> [KeyValue; 3] {
        [
            KeyValue::new("result", self.result),
            KeyValue::new("reason", self.reason),
            KeyValue::new("trigger", self.trigger),
        ]
    }

    pub fn record(self) {
        FS_RECOVERY_RUNS.add(1, &self.attributes());
    }

    pub fn record_with_duration(self, took: Duration) {
        let attributes = self.attributes();
        FS_RECOVERY_RUNS.add(1, &attributes);
        FS_RECOVERY_DURATION.record(took.as_millis() as u64, &attributes);
    }
}

pub fn record_fs_recovery_tooling_unsupported() {
    FS_RECOVERY_TOOLING_UNSUPPORTED.add(1, &[]);
}

static OFFLINE_UPGRADE_ATTEMPTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.envd.offline_upgrade.attempts")
        .with_description("Cold-boot offline envd rootfs-swap attempts, by result and from/to version")
        .with_unit("{attempt}")
        .build()
});

static OFFLINE_UPGRADE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.envd.offline_upgrade.duration")
        .with_description("Wall-time of the offline cold-boot envd rootfs swap (jailed debugfs)")
        .with_unit("ms")
        .build()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineUpgradeAttempt<'a> {
    pub result: &'a str,
    pub from_version: &'a str,
    pub to_version: &'a str,
    pub refire: Option<bool>,
}

impl OfflineUpgradeAttempt<'_> {
    pub fn record(&self) {
        let attributes: Vec<KeyValue> = [
            KeyValue::new("result", self.result.to_owned()),
            KeyValue::new("from_version", self.from_version.to_owned()),
            KeyValue::new("to_version", self.to_version.to_owned()),
        ]
        .into_iter()
        .chain(self.refire.map(|refire| KeyValue::new("refire", refire)))
        .collect();
        OFFLINE_UPGRADE_ATTEMPTS.add(1, &attributes);
    }

    pub fn record_with_duration(&self, took: Duration) {
        self.record();
        OFFLINE_UPGRADE_DURATION.record(
            took.as_millis() as u64,
            &[KeyValue::new("result", self.result.to_owned())],
        );
    }
}
