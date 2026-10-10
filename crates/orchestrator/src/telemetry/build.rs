use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/metrics";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("template.build.duration")
        .with_description("Time taken to build a template")
        .with_unit("ms")
        .build()
});

static PHASE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("template.build.phase.duration")
        .with_description("Time taken to build each phase of a template")
        .with_unit("ms")
        .build()
});

static RESULT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("template.build.result")
        .with_description("Number of template build results")
        .with_unit("{build}")
        .build()
});

static CACHE_RESULT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("template.build.cache.result")
        .with_description("Number of build cache results")
        .with_unit("{layer}")
        .build()
});

static ROOTFS_SIZE: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("template.build.rootfs.size")
        .with_description("Size of the built template rootfs in bytes")
        .with_unit("{By}")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildResult {
    Success,
    UserError,
    InternalError,
}

impl BuildResult {
    pub fn label(self) -> &'static str {
        match self {
            BuildResult::Success => "success",
            BuildResult::UserError => "user_error",
            BuildResult::InternalError => "internal_error",
        }
    }
}

fn team(team_id: &str) -> KeyValue {
    KeyValue::new("team.id", team_id.to_owned())
}

fn millis(took: Duration) -> u64 {
    took.as_millis() as u64
}

pub fn record_build_duration(team_id: &str, took: Duration, success: bool) {
    DURATION.record(millis(took), &[KeyValue::new("success", success), team(team_id)]);
}

pub fn record_phase_duration(team_id: &str, took: Duration, phase: &str, step_type: &str, cached: bool) {
    PHASE_DURATION.record(
        millis(took),
        &[
            KeyValue::new("phase", phase.to_owned()),
            KeyValue::new("step_type", step_type.to_owned()),
            KeyValue::new("cached", cached),
            team(team_id),
        ],
    );
}

pub fn record_build_result(team_id: &str, result: BuildResult) {
    RESULT.add(1, &[KeyValue::new("result", result.label()), team(team_id)]);
}

pub fn record_cache_result(team_id: &str, phase: &str, step_type: &str, hit: bool) {
    CACHE_RESULT.add(
        1,
        &[
            KeyValue::new("phase", phase.to_owned()),
            KeyValue::new("step_type", step_type.to_owned()),
            KeyValue::new("hit", hit),
            team(team_id),
        ],
    );
}

pub fn record_rootfs_size(team_id: &str, bytes: u64) {
    ROOTFS_SIZE.record(bytes, &[team(team_id)]);
}
