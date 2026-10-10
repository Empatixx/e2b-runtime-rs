use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/fc";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static VCPU_LIMIT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.vcpu_limit")
        .with_description(
            "Starts whose VM has spare vCPUs, by how the CPU limit was applied (outcome=confined|cgroup_fallback|failed) on which path (path=boot|resume)",
        )
        .with_unit("{start}")
        .build()
});

pub fn record_vcpu_limit(path: &str, outcome: &str) {
    VCPU_LIMIT.add(
        1,
        &[
            KeyValue::new("path", path.to_owned()),
            KeyValue::new("outcome", outcome.to_owned()),
        ],
    );
}
