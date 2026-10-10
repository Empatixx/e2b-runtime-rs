use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Histogram;

use super::millis;

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/network";
pub const SLOT_RETURN_DURATION: &str = "orchestrator.network.slot.return.duration";
pub const EGRESS_RETIREMENT_DURATION: &str = "orchestrator.network.egress.retirement.duration";

static SLOT_RETURN: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .u64_histogram(SLOT_RETURN_DURATION)
        .with_description("Network slot return attempt duration, by network_version, phase (total including reuse delay or cleanup after it), and result (success, shutdown, retained, error)")
        .with_unit("ms")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotReturnResult {
    Success,
    Shutdown,
    Retained,
    Error,
}

impl SlotReturnResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Shutdown => "shutdown",
            Self::Retained => "retained",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotReturn {
    pub network_version: u8,
    pub result: SlotReturnResult,
    pub total: Duration,
    pub cleanup: Duration,
}

impl SlotReturn {
    pub fn record(&self) {
        [("total", self.total), ("cleanup", self.cleanup)]
            .into_iter()
            .for_each(|(phase, took)| SLOT_RETURN.record(millis(took), &self.attributes(phase)));
    }

    fn attributes(&self, phase: &'static str) -> [KeyValue; 3] {
        [
            KeyValue::new("network_version", i64::from(self.network_version)),
            KeyValue::new("result", self.result.as_str()),
            KeyValue::new("phase", phase),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_phases_carry_the_version_and_the_result() {
        let slot_return = SlotReturn {
            network_version: 2,
            result: SlotReturnResult::Shutdown,
            total: Duration::from_secs(1),
            cleanup: Duration::from_millis(200),
        };

        assert_eq!(
            slot_return.attributes("cleanup"),
            [
                KeyValue::new("network_version", 2),
                KeyValue::new("result", "shutdown"),
                KeyValue::new("phase", "cleanup")
            ]
        );
    }

    #[test]
    fn the_results_read_like_upstream() {
        let names: Vec<_> = [
            SlotReturnResult::Success,
            SlotReturnResult::Shutdown,
            SlotReturnResult::Retained,
            SlotReturnResult::Error,
        ]
        .map(SlotReturnResult::as_str)
        .into();

        assert_eq!(names, ["success", "shutdown", "retained", "error"]);
    }
}
