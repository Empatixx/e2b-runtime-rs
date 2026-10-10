use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/uffd/userfaultfd";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static COW_CAPTURE: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.uffd.cow_capture")
        .with_description("Pages captured into a CoW memory-export window, by capture path")
        .with_unit("{page}")
        .build()
});

static COW_CANCEL: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.uffd.cow_window_cancel")
        .with_description("CoW memory-export windows canceled, by reason")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    Sweep,
    Fault,
}

impl CaptureSource {
    pub fn label(self) -> &'static str {
        match self {
            CaptureSource::Sweep => "sweep",
            CaptureSource::Fault => "fault",
        }
    }
}

pub fn record_cow_capture(source: CaptureSource) {
    COW_CAPTURE.add(1, &[KeyValue::new("source", source.label())]);
}

pub fn record_cow_cancel(reason: &str) {
    COW_CANCEL.add(1, &[KeyValue::new("reason", reason.to_owned())]);
}
