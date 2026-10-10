use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static CALLS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.envd.init.calls")
        .with_description("Number of envd initialization calls")
        .with_unit("1")
        .build()
});

static DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.envd.init.duration")
        .with_description("Time taken for Envd to initialize successfully")
        .with_unit("ms")
        .build()
});

static STARTUP_PAGES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.uffd.startup.pages")
        .with_description("Demand-fault pages a guest needed to reach a successful envd init, per start")
        .with_unit("{page}")
        .build()
});

static STARTUP_SOURCE_PAGES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.uffd.startup.source_pages")
        .with_description("Subset of startup demand-fault pages pulled from the source (e.g. GCS), per start")
        .with_unit("{page}")
        .build()
});

static STARTUP_BYTES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.uffd.startup.bytes")
        .with_description("Bytes faulted into a guest to reach a successful envd init, per start")
        .with_unit("{By}")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitType {
    Success,
    Timeout,
    Canceled,
    Other,
    Transient,
}

impl ExitType {
    pub fn label(self) -> &'static str {
        match self {
            ExitType::Success => "success",
            ExitType::Timeout => "timeout",
            ExitType::Canceled => "canceled",
            ExitType::Other => "other",
            ExitType::Transient => "transient",
        }
    }

    pub fn of(error: Option<&str>) -> Self {
        match error {
            None => ExitType::Success,
            Some(error) if error.contains("syncing took too long") || error.contains("deadline exceeded") => {
                ExitType::Timeout
            }
            Some(error) if error.contains("context canceled") => ExitType::Canceled,
            Some(_) => ExitType::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitCall {
    pub envd_version: String,
    pub timeout_ms: i64,
    pub start_type: &'static str,
    pub team_id: String,
}

impl InitCall {
    fn attributes(&self, exit: ExitType) -> Vec<KeyValue> {
        vec![
            KeyValue::new("envd.version", self.envd_version.clone()),
            KeyValue::new("timeout_ms", self.timeout_ms),
            KeyValue::new("start_type", self.start_type),
            KeyValue::new("team.id", self.team_id.clone()),
            KeyValue::new("success", exit == ExitType::Success),
            KeyValue::new("exit_type", exit.label()),
        ]
    }

    pub fn count(&self, exit: ExitType, calls: u64) {
        CALLS.add(calls, &self.attributes(exit));
    }

    pub fn record_duration(&self, exit: ExitType, took: Duration) {
        DURATION.record(took.as_millis() as u64, &self.attributes(exit));
    }

    pub fn record_startup(&self, success: bool, pages: i64, source_pages: i64, bytes: i64) {
        let attributes = [
            KeyValue::new("start_type", self.start_type),
            KeyValue::new("success", success),
            KeyValue::new("team.id", self.team_id.clone()),
        ];
        STARTUP_PAGES.record(pages.max(0) as u64, &attributes);
        STARTUP_SOURCE_PAGES.record(source_pages.max(0) as u64, &attributes);
        STARTUP_BYTES.record(bytes.max(0) as u64, &attributes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_classified_like_upstream() {
        assert_eq!(ExitType::of(None), ExitType::Success);
        assert_eq!(ExitType::of(Some("syncing took too long")), ExitType::Timeout);
        assert_eq!(ExitType::of(Some("context deadline exceeded")), ExitType::Timeout);
        assert_eq!(ExitType::of(Some("context canceled")), ExitType::Canceled);
        assert_eq!(ExitType::of(Some("fc process exited prematurely")), ExitType::Other);
        assert_eq!(ExitType::of(Some("unexpected status code: 500")), ExitType::Other);
    }

    #[test]
    fn the_attributes_carry_the_team_and_the_exit_type() {
        let call = InitCall {
            envd_version: "0.6.13".to_owned(),
            timeout_ms: 50,
            start_type: "resume",
            team_id: "team-1".to_owned(),
        };

        let attributes = call.attributes(ExitType::Timeout);

        let value = |key: &str| {
            attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.to_string())
        };
        assert_eq!(value("team.id").as_deref(), Some("team-1"));
        assert_eq!(value("exit_type").as_deref(), Some("timeout"));
        assert_eq!(value("success").as_deref(), Some("false"));
        assert_eq!(value("start_type").as_deref(), Some("resume"));
        assert_eq!(value("envd.version").as_deref(), Some("0.6.13"));
        assert_eq!(value("timeout_ms").as_deref(), Some("50"));
    }
}
