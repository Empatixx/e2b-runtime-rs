use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::memory::balloon_mode::BalloonMode;
use crate::memory::page_tracker::PageState;

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

static LEG_FAULTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.template_leg_faults")
        .with_description("Sandbox reads and writes its template's backing device failed (leg=memfile|rootfs)")
        .with_unit("{fault}")
        .build()
});

static WP_RESOLVE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.uffd.wp_resolve")
        .with_description(
            "Time to resolve a synchronous userfault write-protect fault, from serve-loop event read to unprotect+wake",
        )
        .with_unit("us")
        .build()
});

static WP_RESOLVE_COUNT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.uffd.wp_resolve")
        .with_description("Synchronous userfault write-protect faults resolved")
        .build()
});

static WP_PROMOTE: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.uffd.wp_promote")
        .with_description("Tracker dirty-promotions from synchronous write-protect faults, by prior page state")
        .build()
});

struct Timer {
    duration: Histogram<u64>,
    bytes: Counter<u64>,
    count: Counter<u64>,
}

impl Timer {
    fn new(name: &'static str, duration: &'static str, bytes: &'static str, count: &'static str) -> Self {
        Self {
            duration: METER
                .u64_histogram(name)
                .with_description(duration)
                .with_unit("ms")
                .build(),
            bytes: METER.u64_counter(name).with_description(bytes).with_unit("By").build(),
            count: METER.u64_counter(name).with_description(count).build(),
        }
    }

    fn record(&self, took: Duration, bytes: u64, attributes: &[KeyValue]) {
        self.duration.record(took.as_millis() as u64, attributes);
        self.bytes.add(bytes, attributes);
        self.count.add(1, attributes);
    }
}

static SERVE: LazyLock<Timer> = LazyLock::new(|| {
    Timer::new(
        "orchestrator.sandbox.uffd.serve",
        "Time to serve a UFFD demand page fault",
        "Bytes installed into the guest by demand-fault serves",
        "UFFD demand-fault serve attempts",
    )
});

static PREFAULT: LazyLock<Timer> = LazyLock::new(|| {
    Timer::new(
        "orchestrator.sandbox.uffd.prefault",
        "Time to prefault a page into the guest",
        "Bytes installed into the guest by prefaults",
        "UFFD prefault attempts",
    )
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageClass {
    New,
    Zero,
    Resident,
    Unknown,
}

impl PageClass {
    pub fn label(self) -> &'static str {
        match self {
            PageClass::New => "new",
            PageClass::Zero => "zero",
            PageClass::Resident => "resident",
            PageClass::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultResult {
    Installed,
    Present,
    Deferred,
    Discarded,
    Error,
    Skipped,
}

impl FaultResult {
    pub fn label(self) -> &'static str {
        match self {
            FaultResult::Installed => "installed",
            FaultResult::Present => "present",
            FaultResult::Deferred => "deferred",
            FaultResult::Discarded => "discarded",
            FaultResult::Error => "error",
            FaultResult::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WpOutcome {
    Resolved,
    ResolvedCow,
    Deferred,
    Closed,
    Error,
    StaleRemoved,
}

impl WpOutcome {
    pub fn label(self) -> &'static str {
        match self {
            WpOutcome::Resolved => "resolved",
            WpOutcome::ResolvedCow => "resolved_cow",
            WpOutcome::Deferred => "deferred",
            WpOutcome::Closed => "closed",
            WpOutcome::Error => "error",
            WpOutcome::StaleRemoved => "stale_removed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationBucket {
    Zero,
    UpToTen,
    UpToHundred,
    UpToThousand,
    Over,
}

impl GenerationBucket {
    pub fn of(generation: u64) -> Self {
        match generation {
            0 => GenerationBucket::Zero,
            1..=10 => GenerationBucket::UpToTen,
            11..=100 => GenerationBucket::UpToHundred,
            101..=1000 => GenerationBucket::UpToThousand,
            _ => GenerationBucket::Over,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GenerationBucket::Zero => "0",
            GenerationBucket::UpToTen => "1-10",
            GenerationBucket::UpToHundred => "11-100",
            GenerationBucket::UpToThousand => "101-1000",
            GenerationBucket::Over => ">1000",
        }
    }
}

pub fn record_serve(
    mode: BalloonMode,
    generation: GenerationBucket,
    class: PageClass,
    result: FaultResult,
    took: Duration,
    bytes: u64,
) {
    SERVE.record(
        took,
        bytes,
        &[
            KeyValue::new("balloon_mode", mode.as_str()),
            KeyValue::new("generation_bucket", generation.label()),
            KeyValue::new("page_class", class.label()),
            KeyValue::new("result", result.label()),
        ],
    );
}

pub fn record_prefault(generation: GenerationBucket, result: FaultResult, took: Duration, bytes: u64) {
    PREFAULT.record(
        took,
        bytes,
        &[
            KeyValue::new("generation_bucket", generation.label()),
            KeyValue::new("result", result.label()),
        ],
    );
}

pub fn record_wp_resolve(mode: BalloonMode, generation: GenerationBucket, outcome: WpOutcome, took: Duration) {
    let attributes = [
        KeyValue::new("balloon_mode", mode.as_str()),
        KeyValue::new("generation_bucket", generation.label()),
        KeyValue::new("result", outcome.label()),
    ];
    WP_RESOLVE_DURATION.record(took.as_micros() as u64, &attributes);
    WP_RESOLVE_COUNT.add(1, &attributes);
}

pub fn record_wp_promote(generation: GenerationBucket, from: PageState) {
    WP_PROMOTE.add(
        1,
        &[
            KeyValue::new("generation_bucket", generation.label()),
            KeyValue::new("from_state", from.label()),
        ],
    );
}

pub fn record_memfile_leg_fault() {
    LEG_FAULTS.add(1, &[KeyValue::new("leg", "memfile")]);
}
