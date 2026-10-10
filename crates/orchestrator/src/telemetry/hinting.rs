use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static RUNS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.fph.run")
        .with_description(
            "Free-page-hinting drains and runs, labeled by phase (pre-pause, periodic) and outcome (ok, not-configured, refused, timeout, timeout-guest-silent, failed, cancelled, skipped-<reason> incl. skipped-host-busy)",
        )
        .with_unit("{run}")
        .build()
});

static STOPS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.fph.stop")
        .with_description(
            "Stops of hinting cycles the host stopped waiting for, labeled by outcome (ok, unacked, failed, skipped-exited, skipped-disabled)",
        )
        .with_unit("{stop}")
        .build()
});

static RUN_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.fph.duration")
        .with_description("Duration of a completed free-page-hinting drain or run, labeled by phase")
        .with_unit("ms")
        .build()
});

static STOP_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.fph.stop.duration")
        .with_description("Duration of the stop of an abandoned hinting cycle, acknowledgement wait included")
        .with_unit("ms")
        .build()
});

pub fn record_hint_run(phase: &'static str, outcome: &'static str, took: Duration) {
    RUNS.add(1, &[KeyValue::new("phase", phase), KeyValue::new("outcome", outcome)]);
    if outcome == "ok" {
        RUN_DURATION.record(took.as_millis() as u64, &[KeyValue::new("phase", phase)]);
    }
}

pub fn record_hint_stop(outcome: &'static str, took: Duration) {
    STOPS.add(1, &[KeyValue::new("outcome", outcome)]);
    if matches!(outcome, "ok" | "unacked") {
        STOP_DURATION.record(took.as_millis() as u64, &[]);
    }
}
