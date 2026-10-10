use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Histogram, Meter};

use crate::storage::header::{DiffMetadata, Header};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static DIFF_BYTES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.snapshot.diff.bytes")
        .with_description("Per-snapshot dirty/empty bytes per file")
        .with_unit("{By}")
        .build()
});

static DIFF_RATIO: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    METER
        .f64_histogram("orchestrator.sandbox.snapshot.diff.ratio")
        .with_description("Per-snapshot dirty/empty as fraction of total mapped size (1.0 = 100%)")
        .with_unit("{1}")
        .build()
});

static TOTAL_BYTES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.snapshot.total.bytes")
        .with_description("Per-snapshot total mapped size of the file")
        .with_unit("{By}")
        .build()
});

pub fn ratio_fraction(numerator: i64, denominator: i64) -> f64 {
    match denominator <= 0 {
        true => 0.0,
        false => (numerator as f64 / denominator as f64).clamp(0.0, 1.0),
    }
}

pub fn record_snapshot_diff(team_id: &str, file_type: &str, diff: &DiffMetadata, original: &Header) {
    let block_size = original.metadata.block_size as i64;
    let total = original.metadata.size as i64;
    let base = [
        KeyValue::new("file_type", file_type.to_owned()),
        KeyValue::new("team.id", team_id.to_owned()),
    ];
    TOTAL_BYTES.record(total.max(0) as u64, &base);
    [
        ("dirty", diff.dirty.len() as i64 * block_size),
        ("empty", diff.empty.len() as i64 * block_size),
    ]
    .into_iter()
    .for_each(|(kind, bytes)| {
        let attributes = [base[0].clone(), KeyValue::new("kind", kind), base[1].clone()];
        DIFF_BYTES.record(bytes.max(0) as u64, &attributes);
        DIFF_RATIO.record(ratio_fraction(bytes, total), &attributes);
    });
}

pub fn record_snapshot_dedup(
    team_id: &str,
    file_type: &str,
    pre: &DiffMetadata,
    post: Option<&DiffMetadata>,
    best_effort: bool,
) {
    let kind = match (post, best_effort) {
        (None, _) => "none",
        (Some(_), true) => "best_effort_dedup",
        (Some(_), false) => "dedup",
    };
    let pre_bytes = pre.dirty.len() as i64 * pre.block_size as i64;
    let savings = post.map_or(0, |post| {
        (pre_bytes - post.dirty.len() as i64 * post.block_size as i64).max(0)
    });
    let attributes = [
        KeyValue::new("file_type", file_type.to_owned()),
        KeyValue::new("kind", kind),
        KeyValue::new("team.id", team_id.to_owned()),
    ];
    DIFF_BYTES.record(savings as u64, &attributes);
    DIFF_RATIO.record(ratio_fraction(savings, pre_bytes), &attributes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ratio_is_a_fraction_clamped_to_zero_and_one() {
        assert_eq!(ratio_fraction(1, 4), 0.25);
        assert_eq!(ratio_fraction(5, 4), 1.0);
        assert_eq!(ratio_fraction(-1, 4), 0.0);
        assert_eq!(ratio_fraction(1, 0), 0.0);
    }
}
