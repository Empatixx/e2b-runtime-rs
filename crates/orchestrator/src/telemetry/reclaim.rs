use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

fn histogram(name: &'static str, description: &'static str, unit: &'static str) -> Histogram<u64> {
    METER
        .u64_histogram(name)
        .with_description(description)
        .with_unit(unit)
        .build()
}

static FREEZE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.envd.freeze.duration",
        "Round-trip duration of the pre-pause workload freeze call, per pause",
        "ms",
    )
});

static FREEZE_SWEEP: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.envd.freeze.sweep",
        "Time envd spent issuing cgroup.freeze writes, per pause (scales with cgroup count)",
        "ms",
    )
});

static FREEZE_WAIT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.envd.freeze.wait",
        "Time envd spent waiting for cgroups to quiesce, per pause (scales with guest I/O depth)",
        "ms",
    )
});

static FREEZE_VISITED: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.envd.freeze.visited",
        "Cgroups the pre-pause freeze walk examined, per pause. Sizes the walk's bound: the bound should sit an order of magnitude above this",
        "{cgroup}",
    )
});

static FREEZE_CGROUPS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.envd.freeze.cgroups",
        "Cgroups affected by a pre-pause freeze, per pause, split by outcome",
        "{cgroup}",
    )
});

static UNFREEZE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.envd.unfreeze.duration",
        "Round-trip duration of the pause-rollback workload thaw call, per rollback",
        "ms",
    )
});

static COLLAPSE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.envd.collapse.duration",
        "Time taken for the pre-pause envd heap collapse round-trip",
        "ms",
    )
});

static COLLAPSE_CHUNKS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.envd.collapse.chunks")
        .with_description("2 MiB chunks the pre-pause envd heap collapse attempted, by result")
        .with_unit("{chunk}")
        .build()
});

fn team(team_id: &str) -> KeyValue {
    KeyValue::new("team.id", team_id.to_owned())
}

fn millis(took: Duration) -> u64 {
    took.as_millis() as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreezeCall {
    pub success: bool,
    pub reported: bool,
    pub timed_out: bool,
    pub timeout: Duration,
}

pub fn record_freeze_call(call: FreezeCall, took: Duration, team_id: &str) {
    FREEZE_DURATION.record(
        millis(took),
        &[
            KeyValue::new("success", call.success),
            KeyValue::new("reported", call.reported),
            KeyValue::new("timed_out", call.timed_out),
            KeyValue::new("timeout_ms", call.timeout.as_millis() as i64),
            team(team_id),
        ],
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeObserved<'a> {
    pub mode: &'a str,
    pub sweep_ms: i64,
    pub wait_ms: i64,
    pub visited: i64,
    pub outcomes: [(&'static str, i64); 6],
}

pub fn record_freeze_observed(observed: &FreezeObserved<'_>) {
    let mode = [KeyValue::new("mode", observed.mode.to_owned())];
    FREEZE_SWEEP.record(observed.sweep_ms.max(0) as u64, &mode);
    FREEZE_WAIT.record(observed.wait_ms.max(0) as u64, &mode);
    FREEZE_VISITED.record(observed.visited.max(0) as u64, &mode);
    observed.outcomes.iter().for_each(|(outcome, count)| {
        FREEZE_CGROUPS.record(
            (*count).max(0) as u64,
            &[
                KeyValue::new("outcome", *outcome),
                KeyValue::new("mode", observed.mode.to_owned()),
            ],
        );
    });
}

pub fn record_unfreeze(success: bool, took: Duration, team_id: &str) {
    UNFREEZE_DURATION.record(millis(took), &[KeyValue::new("success", success), team(team_id)]);
}

pub fn record_collapse_duration(success: bool, took: Duration, team_id: &str) {
    COLLAPSE_DURATION.record(millis(took), &[KeyValue::new("success", success), team(team_id)]);
}

pub fn record_collapse_chunks(collapsed: u64, already_huge: u64, skipped: u64) {
    [
        ("collapsed", collapsed),
        ("already_huge", already_huge),
        ("skipped", skipped),
    ]
    .into_iter()
    .for_each(|(result, count)| COLLAPSE_CHUNKS.add(count, &[KeyValue::new("result", result)]));
}
