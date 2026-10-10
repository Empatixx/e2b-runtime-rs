use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static APPLIED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.envd.defaults.applied")
        .with_description(
            "Memory resumes by where the envd default user the orchestrator sent was derived from, and sandbox type",
        )
        .with_unit("{resume}")
        .build()
});

static WORKDIR_WITHHELD: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.envd.defaults.workdir_withheld")
        .with_description(
            "Memory resumes where a recorded default workdir could not be proven and was not re-sent (upper bound on the unrepaired population)",
        )
        .with_unit("{resume}")
        .build()
});

static MISMATCH: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.envd.defaults.mismatch")
        .with_description("/init responses where envd's effective defaults differ from what was sent, by field")
        .with_unit("{mismatch}")
        .build()
});

static BUILTIN_FALLBACK: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.envd.defaults.builtin_fallback")
        .with_description(
            "/init responses where envd reports it was never told which user to run as, by whether one was sent and sandbox type",
        )
        .with_unit("{response}")
        .build()
});

pub fn record_applied(source: &str, sandbox_type: &str, workdir_withheld: bool) {
    let attributes = [
        KeyValue::new("source", source.to_owned()),
        KeyValue::new("sandbox_type", sandbox_type.to_owned()),
    ];
    APPLIED.add(1, &attributes);
    if workdir_withheld {
        WORKDIR_WITHHELD.add(1, &attributes);
    }
}

pub fn record_mismatch(field: &str) {
    MISMATCH.add(1, &[KeyValue::new("field", field.to_owned())]);
}

pub fn record_builtin_fallback(sent: bool, sandbox_type: &str) {
    BUILTIN_FALLBACK.add(
        1,
        &[
            KeyValue::new("sent", sent),
            KeyValue::new("sandbox_type", sandbox_type.to_owned()),
        ],
    );
}
