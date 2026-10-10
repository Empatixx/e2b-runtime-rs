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

static FREED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.fph.freed_bytes")
        .with_description(
            "Bytes discarded by completed free-page-hinting drains and runs (FC free_page_hint_freed before the drain and after a metrics flush that follows it), labeled by phase",
        )
        .with_unit("By")
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

static LATCHES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.fph.latch")
        .with_description(
            "Guests the periodic hinter latched as unable to hint after consecutive sat-out runs, released by a completed run, or dropped with the loop that held the latch (state: latched, released, dropped)",
        )
        .with_unit("{transition}")
        .build()
});

static RUN_FAULTS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.fph.run.faults")
        .with_description(
            "Demand faults the serve loop resolved for one sandbox across a periodic hinting attempt (window=run) and over the interval that followed a tick (window=interval), labeled by kind (served pages, deferred installs, wp faults, deferred wp resolves) and outcome: the attempt's run outcome, or for a tick that attempted nothing the skip reason or observe-only, which is the baseline an attempt is read against",
        )
        .with_unit("{fault}")
        .build()
});

pub fn record_hint_run(phase: &'static str, outcome: &'static str, took: Duration) {
    RUNS.add(1, &[KeyValue::new("phase", phase), KeyValue::new("outcome", outcome)]);
    if outcome == "ok" {
        RUN_DURATION.record(took.as_millis() as u64, &[KeyValue::new("phase", phase)]);
    }
}

pub fn record_hint_freed(phase: &'static str, freed: u64) {
    if freed > 0 {
        FREED.add(freed, &[KeyValue::new("phase", phase)]);
    }
}

pub fn record_hint_stop(outcome: &'static str, took: Duration) {
    STOPS.add(1, &[KeyValue::new("outcome", outcome)]);
    if matches!(outcome, "ok" | "unacked") {
        STOP_DURATION.record(took.as_millis() as u64, &[]);
    }
}

pub fn record_hint_latch(state: &'static str) {
    LATCHES.add(1, &[KeyValue::new("state", state)]);
}

pub fn record_hint_faults(window: &'static str, outcome: &'static str, kind: &'static str, count: i64) {
    RUN_FAULTS.record(
        count.max(0) as u64,
        &[
            KeyValue::new("kind", kind),
            KeyValue::new("window", window),
            KeyValue::new("outcome", outcome),
        ],
    );
}
