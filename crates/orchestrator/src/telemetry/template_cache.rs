use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/template";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static HITS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.templates.cache.hits")
        .with_description("Requests for templates that were already cached")
        .build()
});

static MISSES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.templates.cache.misses")
        .with_description("Requests for templates that were not cached (reason=insert|cold|released_layer)")
        .build()
});

static SUPERSEDED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.templates.cache.snapshot_layers_superseded")
        .with_description("Pause layers marked superseded, once per layer")
        .with_unit("{layer}")
        .build()
});

static OUTCOMES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.templates.cache.snapshot_layer_outcome")
        .with_description("Superseded pause layers by terminal outcome (outcome=released|expired_held), once per layer")
        .with_unit("{layer}")
        .build()
});

static SUPPRESSED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.templates.cache.snapshot_layer_release_suppressed")
        .with_description("Superseded pause layers that would have been released with the release off, once per layer")
        .with_unit("{layer}")
        .build()
});

static MEMFILE_DEDUP_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.memfile_dedup.duration")
        .with_description(
            "Background memfile dedup latency, from the provisional header's creation at pause to the durable-header swap",
        )
        .with_unit("ms")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
    Insert,
    Cold,
    ReleasedLayer,
}

impl MissReason {
    fn label(self) -> &'static str {
        match self {
            MissReason::Insert => "insert",
            MissReason::Cold => "cold",
            MissReason::ReleasedLayer => "released_layer",
        }
    }
}

pub fn record_hit() {
    HITS.add(1, &[]);
}

pub fn record_miss(reason: MissReason) {
    MISSES.add(1, &[KeyValue::new("reason", reason.label())]);
}

pub fn record_superseded() {
    SUPERSEDED.add(1, &[]);
}

pub fn record_released() {
    OUTCOMES.add(1, &[KeyValue::new("outcome", "released")]);
}

pub fn record_expired_held() {
    OUTCOMES.add(1, &[KeyValue::new("outcome", "expired_held")]);
}

pub fn record_suppressed() {
    SUPPRESSED.add(1, &[]);
}

pub fn record_memfile_dedup_duration(took: std::time::Duration) {
    MEMFILE_DEDUP_DURATION.record(took.as_millis() as u64, &[]);
}
