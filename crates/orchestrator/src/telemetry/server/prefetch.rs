use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use super::{METER, counter, millis, milliseconds};

static ATTEMPTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.pause_resume_prefetch.harvest.attempts",
        "Pause-resume prefetch harvest attempts, by result",
        "{attempt}",
    )
});

static DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause_resume_prefetch.harvest.duration",
        "Time the pause-resume prefetch harvest held a start slot (throwaway resume, trace collection, reap)",
    )
});

static PAGES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("orchestrator.sandbox.pause_resume_prefetch.harvest.pages")
        .with_description("Harvested resume-prefetch trace size in 2 MiB blocks, per successful harvest")
        .with_unit("{page}")
        .build()
});

static SEAL_WAIT_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause_resume_prefetch.seal_wait.duration",
        "Time the prefetch harvest waited for the deferred rootfs seal before its warm resume",
    )
});

static PERSIST_WAIT_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause_resume_prefetch.persist_wait.duration",
        "Time the prefetch harvest waited for the in-flight snapshot upload before persisting the mapping",
    )
});

pub fn record_seal_wait(took: Duration) {
    SEAL_WAIT_DURATION.record(millis(took), &[]);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarvestRun {
    pub result: String,
    pub slot_hold: Duration,
    pub persist_wait: Option<Duration>,
    pub pages: Option<u64>,
}

impl HarvestRun {
    pub fn record(&self) {
        let result = [KeyValue::new("result", self.result.clone())];
        ATTEMPTS.add(1, &result);
        DURATION.record(millis(self.slot_hold), &result);
        self.persist_wait
            .iter()
            .for_each(|waited| PERSIST_WAIT_DURATION.record(millis(*waited), &result));
        self.pages.iter().for_each(|pages| PAGES.record(*pages, &[]));
    }
}
