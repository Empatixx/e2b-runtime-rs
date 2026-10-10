use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static WP_MODE: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.resume.wp_mode")
        .with_description("Sandbox resumes by write-protect tracking mode (sync|async)")
        .with_unit("{resume}")
        .build()
});

pub fn record_wp_mode(sync: bool) {
    let mode = if sync { "sync" } else { "async" };
    WP_MODE.add(1, &[KeyValue::new("mode", mode)]);
}
