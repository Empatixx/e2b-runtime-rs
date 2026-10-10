pub mod envd_upgrade;
pub mod gauges;
pub mod network;
pub mod prefetch;
pub mod sandbox;
pub mod spans;

use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/server";

pub(crate) static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static SNAPSHOT_UPLOAD_FAILED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.snapshot.upload.failed")
        .with_description("Number of pause-snapshot uploads that never landed durably")
        .with_unit("{snapshot}")
        .build()
});

pub fn count_failed_snapshot_upload(filesystem_only: bool) {
    SNAPSHOT_UPLOAD_FAILED.add(1, &[KeyValue::new("fs_only", filesystem_only)]);
}

pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn counter(name: &'static str, description: &'static str, unit: &'static str) -> Counter<u64> {
    METER
        .u64_counter(name)
        .with_description(description)
        .with_unit(unit)
        .build()
}

pub(crate) fn milliseconds(name: &'static str, description: &'static str) -> Histogram<u64> {
    METER
        .u64_histogram(name)
        .with_description(description)
        .with_unit("ms")
        .build()
}
