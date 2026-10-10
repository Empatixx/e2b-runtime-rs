use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/block";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static INFLIGHT_SERVE_PAGES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.memfd.inflight_serve_pages")
        .with_description("Guest pages served from the still-mapped memfd during dedup (in-flight memfd serving)")
        .build()
});

static SWAP_GRACE_ELAPSED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.memfd.swap_grace_elapsed")
        .with_description("Dedup memfd releases that timed out on the swap grace without a swap signal")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServePhase {
    Provisional,
    Drain,
}

impl ServePhase {
    fn label(self) -> &'static str {
        match self {
            ServePhase::Provisional => "provisional",
            ServePhase::Drain => "drain",
        }
    }
}

pub fn record_inflight_serve(phase: ServePhase, pages: u64) {
    INFLIGHT_SERVE_PAGES.add(pages, &[KeyValue::new("phase", phase.label())]);
}

pub fn record_swap_grace_elapsed() {
    SWAP_GRACE_ELAPSED.add(1, &[]);
}
