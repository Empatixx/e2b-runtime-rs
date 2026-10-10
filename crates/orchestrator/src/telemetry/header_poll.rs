use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/build";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static HEADER_POLL_WAIT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.storage.upload.header_poll_wait")
        .with_description("Duration of the upload-side wait for the finalized V4 header to appear")
        .with_unit("ms")
        .build()
});

pub fn record_header_poll_wait(file_type: &str, result: &str, took: Duration) {
    HEADER_POLL_WAIT.record(
        took.as_millis() as u64,
        &[
            KeyValue::new("file_type", file_type.to_owned()),
            KeyValue::new("result", result.to_owned()),
        ],
    );
}
