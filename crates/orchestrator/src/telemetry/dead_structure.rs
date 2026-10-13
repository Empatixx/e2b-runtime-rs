use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Counter;

const BLOCK_METER: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/block";
const TEMPLATE_METER: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/template";
const SANDBOX_METER: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";
const OUTCOME_NAME: &str = "orchestrator.sandbox.snapshot.dead_structure_outcome";
const BYTES_NAME: &str = "orchestrator.sandbox.snapshot.dead_structure_bytes";

static BLOCK_OUTCOMES: LazyLock<Counter<u64>> = LazyLock::new(|| outcome_counter(BLOCK_METER));
static BLOCK_BYTES: LazyLock<Counter<u64>> = LazyLock::new(|| bytes_counter(BLOCK_METER));
static TEMPLATE_OUTCOMES: LazyLock<Counter<u64>> = LazyLock::new(|| outcome_counter(TEMPLATE_METER));
static TEMPLATE_BYTES: LazyLock<Counter<u64>> = LazyLock::new(|| bytes_counter(TEMPLATE_METER));
static SANDBOX_OUTCOMES: LazyLock<Counter<u64>> = LazyLock::new(|| outcome_counter(SANDBOX_METER));

fn outcome_counter(meter: &'static str) -> Counter<u64> {
    global::meter(meter)
        .u64_counter(OUTCOME_NAME)
        .with_description("Passes through a per-pause structure's drop site, by structure and outcome")
        .with_unit("{pass}")
        .build()
}

fn bytes_counter(meter: &'static str) -> Counter<u64> {
    global::meter(meter)
        .u64_counter(BYTES_NAME)
        .with_description("Bytes of per-pause structures dropped or kept at their drop site, by structure and outcome")
        .with_unit("By")
        .build()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropOutcome {
    Dropped,
    FlagOff,
    Nothing,
}

impl DropOutcome {
    pub fn of(present: bool, drop: bool) -> Self {
        match (present, drop) {
            (false, _) => DropOutcome::Nothing,
            (true, true) => DropOutcome::Dropped,
            (true, false) => DropOutcome::FlagOff,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DropOutcome::Dropped => "dropped",
            DropOutcome::FlagOff => "flag_off",
            DropOutcome::Nothing => "none",
        }
    }
}

fn attributes(structure: &'static str, outcome: &'static str) -> [KeyValue; 2] {
    [KeyValue::new("structure", structure), KeyValue::new("outcome", outcome)]
}

fn record_with_bytes(
    outcomes: &Counter<u64>,
    bytes: &Counter<u64>,
    structure: &'static str,
    outcome: DropOutcome,
    size: u64,
) {
    let attributes = attributes(structure, outcome.label());
    outcomes.add(1, &attributes);
    if outcome != DropOutcome::Nothing {
        bytes.add(size, &attributes);
    }
}

pub fn record_dedup_index(outcome: DropOutcome, size: u64) {
    record_with_bytes(&BLOCK_OUTCOMES, &BLOCK_BYTES, "dedup_index", outcome, size);
}

pub fn record_template_provisional_header(outcome: DropOutcome, size: u64) {
    record_with_bytes(
        &TEMPLATE_OUTCOMES,
        &TEMPLATE_BYTES,
        "template_provisional_header",
        outcome,
        size,
    );
}

pub fn record_upload_provisional_header(outcome: DropOutcome) {
    SANDBOX_OUTCOMES.add(1, &attributes("upload_provisional_header", outcome.label()));
}

pub fn record_upload_provisional_header_misordered() {
    TEMPLATE_OUTCOMES.add(1, &attributes("upload_provisional_header", "misordered"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outcome_follows_presence_then_the_flag() {
        assert_eq!(DropOutcome::of(false, true), DropOutcome::Nothing);
        assert_eq!(DropOutcome::of(true, true), DropOutcome::Dropped);
        assert_eq!(DropOutcome::of(true, false), DropOutcome::FlagOff);
    }
}
