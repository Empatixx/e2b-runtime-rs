use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use super::{counter, millis, milliseconds, team};

static GUEST_SYNC_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.guest_sync.duration",
        "Time taken for the mandatory pre-pause guest sync (filesystem-only pause)",
    )
});

static FSTHAW_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.envd.fsthaw.duration",
        "Round-trip duration of a rootfs thaw attempt after an in-place resume of a filesystem-only checkpoint (native /fsthaw or fsfreeze -u via exec), labeled by attempt and success; a final failure tears the sandbox down",
    )
});

static GUEST_FREEZE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.snapshot.guest_freeze.duration",
        "Wall time the guest is frozen during an in-place checkpoint, from the FC pause call to the in-place resume, labeled by deferred, fs_only (filesystem-only, no memory export), balloon_mode and success; success=false means the resume ran on the pause-failure cleanup path",
    )
});

static PROCESS_ROOTFS_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.snapshot.process_rootfs.duration",
        "Time to export+diff the rootfs during a pause snapshot, labeled by fs_only and success",
    )
});

static FS_QUIESCED_PAUSE: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.pause.fs_quiesced",
        "Filesystem-only pauses by whether the rootfs was frozen (quiesced) vs sync fallback",
        "{pause}",
    )
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSyncOutcome {
    pub success: bool,
    pub method: String,
    pub froze: bool,
    pub timeout: Duration,
    pub team_id: String,
}

impl GuestSyncOutcome {
    pub fn record(&self, took: Duration) {
        GUEST_SYNC_DURATION.record(
            millis(took),
            &[
                KeyValue::new("success", self.success),
                KeyValue::new("fsfreeze", self.froze),
                KeyValue::new("method", self.method.clone()),
                KeyValue::new(
                    "timeout_ms",
                    i64::try_from(self.timeout.as_millis()).unwrap_or(i64::MAX),
                ),
                team(&self.team_id),
            ],
        );
    }
}

pub fn record_fsthaw(attempt: u32, success: bool, took: Duration, team_id: &str) {
    FSTHAW_DURATION.record(
        millis(took),
        &[
            KeyValue::new("success", success),
            KeyValue::new("attempt", i64::from(attempt)),
            team(team_id),
        ],
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestFreezeOutcome {
    pub deferred: bool,
    pub fs_only: bool,
    pub balloon_mode: String,
    pub success: bool,
    pub team_id: String,
}

impl GuestFreezeOutcome {
    pub fn record(&self, took: Duration) {
        GUEST_FREEZE_DURATION.record(
            millis(took),
            &[
                KeyValue::new("deferred", self.deferred),
                KeyValue::new("fs_only", self.fs_only),
                KeyValue::new("balloon_mode", self.balloon_mode.clone()),
                KeyValue::new("success", self.success),
                team(&self.team_id),
            ],
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRootfsOutcome {
    pub fs_only: bool,
    pub deferred: bool,
    pub in_place: bool,
    pub success: bool,
    pub team_id: String,
}

impl ProcessRootfsOutcome {
    pub fn record(&self, took: Duration) {
        PROCESS_ROOTFS_DURATION.record(
            millis(took),
            &[
                KeyValue::new("fs_only", self.fs_only),
                KeyValue::new("deferred", self.deferred),
                KeyValue::new("in_place", self.in_place),
                KeyValue::new("success", self.success),
                team(&self.team_id),
            ],
        );
    }
}

pub fn count_fs_quiesced_pause(quiesced: bool) {
    FS_QUIESCED_PAUSE.add(1, &[KeyValue::new("quiesced", quiesced)]);
}

static FPR_RESUME: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.fpr_resume",
        "Free-page-reporting resumes after a CoW window, labeled by outcome (inline, retry, fenced, fc_exited, abandoned); produced only while in-place-checkpoint-reporting admits reporting balloons to the in-place path",
        "{resume}",
    )
});

static PROCESS_MEMORY_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.snapshot.process_memory.duration",
        "Time to export+diff the memory file during a pause snapshot (memory pauses only), labeled by in_place, deferred, balloon_mode and success",
    )
});

static MEMORY_SEAL_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.snapshot.memory_seal.duration",
        "Time for the background CoW-window memory capture (off the in-place resume critical path), labeled by success",
    )
});

pub fn record_fpr_resume(outcome: &str) {
    FPR_RESUME.add(1, &[KeyValue::new("outcome", outcome.to_owned())]);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessMemoryOutcome {
    pub in_place: bool,
    pub deferred: bool,
    pub balloon_mode: String,
    pub success: bool,
    pub team_id: String,
}

impl ProcessMemoryOutcome {
    pub fn record(&self, took: Duration) {
        PROCESS_MEMORY_DURATION.record(
            millis(took),
            &[
                KeyValue::new("in_place", self.in_place),
                KeyValue::new("deferred", self.deferred),
                KeyValue::new("balloon_mode", self.balloon_mode.clone()),
                KeyValue::new("success", self.success),
                team(&self.team_id),
            ],
        );
    }
}

pub fn record_memory_seal(success: bool, took: Duration, team_id: &str) {
    MEMORY_SEAL_DURATION.record(millis(took), &[KeyValue::new("success", success), team(team_id)]);
}
