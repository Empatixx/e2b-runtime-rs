use std::sync::LazyLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use super::{counter, millis, milliseconds, team};

static CREATE_DURATION: LazyLock<Histogram<u64>> =
    LazyLock::new(|| milliseconds("orchestrator.sandbox.create.duration", "Time taken to create a sandbox"));

static EXECUTION_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.execution.duration",
        "Time a single sandbox execution ran, from the guest being ready until it stopped executing, labeled by stop reason",
    )
});

static PAUSE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause.duration",
        "Time taken to pause a sandbox, labeled by fs_only (filesystem-only vs memory) and success",
    )
});

static CHECKPOINT_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.checkpoint.duration",
        "Wall time of one Checkpoint RPC from the route decision to the reply, labeled by in_place, fs_only, route, balloon_mode, deferred and success: the per-route latency the checkpoint counter only counts, with deferred separating the CoW-window and synchronous-copy arms of an in-place reporting checkpoint",
    )
});

static PAUSE_ADMISSION_WAIT_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    milliseconds(
        "orchestrator.sandbox.pause_admission.wait.duration",
        "Time snapshot admission waited on the durable parent header whenever it waited, labeled by outcome (ready_after_wait/refused)",
    )
});

static KILLED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.killed",
        "Number of sandboxes killed, labeled by kill reason",
        "{sandbox}",
    )
});

static CRASHED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.crashed",
        "Sandbox executions that ended without a recorded stop reason, labeled by cause (clean_exit/exit_error/external_signal/fault/memory_handler_failed/requested_signal/unknown)",
        "{sandbox}",
    )
});

static CHECKPOINT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.checkpoint",
        "Number of sandbox checkpoints taken, labeled by in_place, fs_only (filesystem-only, no memory export; always in place), route (in_place, or the gate condition that sent it resume-fresh: sync_wp_off, flag_off, fc_unsupported, balloon_reporting, balloon_unknown), balloon_mode, deferred (in-place memory export through the CoW window rather than the synchronous copy; always false on resume-fresh) and success",
        "{checkpoint}",
    )
});

static PAUSE_ADMISSION: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.pause_admission",
        "Snapshot-admission decisions, labeled by outcome (ready/ready_after_wait/refused/latched_error) and rpc (pause/checkpoint)",
        "{decision}",
    )
});

const UNKNOWN_KILL_REASON: &str = "unknown";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOutcome {
    pub resume: bool,
    pub fs_only: bool,
    pub fs_boot_requested: bool,
    pub success: bool,
    pub envd_upgraded: bool,
    pub fs_recovery: String,
    pub team_id: String,
}

impl CreateOutcome {
    pub fn record(&self, took: Duration) {
        CREATE_DURATION.record(millis(took), &self.attributes());
    }

    fn attributes(&self) -> [KeyValue; 7] {
        [
            KeyValue::new("sandbox.resume", self.resume),
            KeyValue::new("fs_only", self.fs_only),
            KeyValue::new("fs_boot_requested", self.fs_boot_requested),
            KeyValue::new("success", self.success),
            KeyValue::new("envd.upgraded", self.envd_upgraded),
            KeyValue::new("fs_recovery", self.fs_recovery.clone()),
            team(&self.team_id),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseOutcome {
    pub fs_only: bool,
    pub success: bool,
    pub team_id: String,
}

impl PauseOutcome {
    pub fn record(&self, took: Duration) {
        PAUSE_DURATION.record(
            millis(took),
            &[
                KeyValue::new("fs_only", self.fs_only),
                KeyValue::new("success", self.success),
                team(&self.team_id),
            ],
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointOutcome {
    pub in_place: bool,
    pub fs_only: bool,
    pub route: String,
    pub balloon_mode: String,
    pub deferred: bool,
    pub success: bool,
    pub team_id: String,
}

impl CheckpointOutcome {
    pub fn record(&self, took: Duration) {
        let attributes = self.attributes();
        CHECKPOINT_DURATION.record(millis(took), &attributes);
        CHECKPOINT.add(1, &attributes);
    }

    fn attributes(&self) -> [KeyValue; 7] {
        [
            KeyValue::new("in_place", self.in_place),
            KeyValue::new("fs_only", self.fs_only),
            KeyValue::new("route", self.route.clone()),
            KeyValue::new("balloon_mode", self.balloon_mode.clone()),
            KeyValue::new("deferred", self.deferred),
            KeyValue::new("success", self.success),
            team(&self.team_id),
        ]
    }
}

pub fn record_kill(team_id: &str, kill_reason: &str) {
    KILLED.add(
        1,
        &[
            KeyValue::new("kill_reason", kill_reason_or_unknown(kill_reason)),
            team(team_id),
        ],
    );
}

fn kill_reason_or_unknown(kill_reason: &str) -> String {
    match kill_reason.is_empty() {
        true => UNKNOWN_KILL_REASON.to_owned(),
        false => kill_reason.to_owned(),
    }
}

pub fn record_execution(team_id: &str, took: Duration, stop_reason: &str) {
    EXECUTION_DURATION.record(
        millis(took),
        &[KeyValue::new("stop_reason", stop_reason.to_owned()), team(team_id)],
    );
}

pub fn record_crash(team_id: &str, cause: &str) {
    CRASHED.add(1, &[KeyValue::new("cause", cause.to_owned()), team(team_id)]);
}

pub fn record_pause_admission(team_id: &str, rpc: &str, outcome: &str, waited: Duration) {
    if outcome.is_empty() {
        return;
    }
    PAUSE_ADMISSION.add(
        1,
        &[
            KeyValue::new("outcome", outcome.to_owned()),
            KeyValue::new("rpc", rpc.to_owned()),
            team(team_id),
        ],
    );
    if matches!(outcome, "ready_after_wait" | "refused") {
        PAUSE_ADMISSION_WAIT_DURATION.record(
            millis(waited),
            &[KeyValue::new("outcome", outcome.to_owned()), team(team_id)],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_kill_reason_is_reported_as_unknown() {
        assert_eq!(kill_reason_or_unknown(""), "unknown");
        assert_eq!(kill_reason_or_unknown("timeout"), "timeout");
    }

    #[test]
    fn the_create_histogram_carries_the_attributes_of_upstream() {
        let outcome = CreateOutcome {
            resume: true,
            fs_only: false,
            fs_boot_requested: false,
            success: true,
            envd_upgraded: false,
            fs_recovery: "none".to_owned(),
            team_id: "team-1".to_owned(),
        };

        let keys: Vec<_> = outcome
            .attributes()
            .iter()
            .map(|attribute| attribute.key.to_string())
            .collect();

        assert_eq!(
            keys,
            [
                "sandbox.resume",
                "fs_only",
                "fs_boot_requested",
                "success",
                "envd.upgraded",
                "fs_recovery",
                "team.id"
            ]
        );
    }

    #[test]
    fn the_checkpoint_counter_and_histogram_share_their_attributes() {
        let outcome = CheckpointOutcome {
            in_place: true,
            fs_only: false,
            route: "in_place".to_owned(),
            balloon_mode: "none".to_owned(),
            deferred: false,
            success: true,
            team_id: "team-1".to_owned(),
        };

        let keys: Vec<_> = outcome
            .attributes()
            .iter()
            .map(|attribute| attribute.key.to_string())
            .collect();

        assert_eq!(
            keys,
            [
                "in_place",
                "fs_only",
                "route",
                "balloon_mode",
                "deferred",
                "success",
                "team.id"
            ]
        );
    }
}
