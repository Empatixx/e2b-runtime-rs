use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use super::{METER, counter, millis, milliseconds, team};

static ATTEMPTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.pause_resume_prefetch.harvest.attempts",
        "Resume prefetch harvest attempts after a pause or an in-place checkpoint, by result (success | resume_failed | collect_failed | skipped = seal did not settle | slot_timeout = no start slot | persist_deadline) and path (pause | checkpoint)",
        "{attempt}",
    )
});

static DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause_resume_prefetch.harvest.duration",
        "Time the resume prefetch harvest held a start slot (throwaway resume, trace collection, reap), by result and path",
    )
});

static PAGES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.pause_resume_prefetch.harvest.pages")
        .with_description("Harvested resume-prefetch trace size in 2 MiB blocks, per successful harvest, by path (pause | checkpoint)")
        .with_unit("{page}")
        .build()
});

static SEAL_WAIT_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause_resume_prefetch.seal_wait.duration",
        "Time the prefetch harvest waited for the deferred seals (in-place memfile, then rootfs) before its warm resume, by result and path",
    )
});

static PERSIST_WAIT_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause_resume_prefetch.persist_wait.duration",
        "Time the prefetch harvest waited for the in-flight snapshot upload before persisting the mapping",
    )
});

pub const PATH_PAUSE: &str = "pause";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarvestRun {
    pub result: String,
    pub path: &'static str,
    pub seal_wait: Duration,
    pub slot_hold: Duration,
    pub persist_wait: Option<Duration>,
    pub pages: Option<u64>,
    pub team_id: String,
}

impl HarvestRun {
    pub fn record(&self) {
        let result = self.result_attributes();
        SEAL_WAIT_DURATION.record(millis(self.seal_wait), &result);
        ATTEMPTS.add(1, &self.attempt_attributes());
        DURATION.record(millis(self.slot_hold), &result);
        self.persist_wait
            .iter()
            .for_each(|waited| PERSIST_WAIT_DURATION.record(millis(*waited), &result));
        self.pages
            .iter()
            .for_each(|pages| PAGES.record(*pages, &[KeyValue::new("path", self.path)]));
    }

    pub fn attempt_attributes(&self) -> [KeyValue; 3] {
        [
            KeyValue::new("result", self.result.clone()),
            KeyValue::new("path", self.path),
            team(&self.team_id),
        ]
    }

    pub fn result_attributes(&self) -> [KeyValue; 2] {
        [
            KeyValue::new("result", self.result.clone()),
            KeyValue::new("path", self.path),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_duration_and_attempt_carries_the_result_and_the_path() {
        let run = HarvestRun {
            result: "slot_timeout".to_owned(),
            path: PATH_PAUSE,
            seal_wait: Duration::ZERO,
            slot_hold: Duration::from_millis(5),
            persist_wait: None,
            pages: None,
            team_id: "team-1".to_owned(),
        };

        assert_eq!(
            run.result_attributes(),
            [KeyValue::new("result", "slot_timeout"), KeyValue::new("path", "pause")]
        );
        assert_eq!(
            run.attempt_attributes(),
            [
                KeyValue::new("result", "slot_timeout"),
                KeyValue::new("path", "pause"),
                KeyValue::new("team.id", "team-1")
            ]
        );
    }
}
