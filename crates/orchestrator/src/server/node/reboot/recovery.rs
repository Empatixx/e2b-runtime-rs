use std::path::PathBuf;
use std::time::Instant;

use futures::future::BoxFuture;
use tokio::sync::OnceCell;
use tracing::field::Empty;
use tracing::{Instrument, info_span};

use crate::sandbox::config::PreBoot;
use crate::sandbox::rootfs::recovery::{
    RecoverOutcome, RecoverReason, RecoveryRecord, journal_only_supported, recover_filesystem,
};
use crate::telemetry::reboot::{FsRecoveryRun, record_fs_recovery_tooling_unsupported};

const TRIGGER_RESCUE: &str = "rescue";
const TRIGGER_LEGACY: &str = "legacy_fs_only";

static JOURNAL_ONLY_PROBE: OnceCell<()> = OnceCell::const_new();

pub async fn probe_journal_only() {
    JOURNAL_ONLY_PROBE
        .get_or_init(|| async {
            if !journal_only_supported().await {
                record_fs_recovery_tooling_unsupported();
                tracing::warn!(
                    "host e2fsck rejects -E journal_only; pre-boot recovery is inert, boots fall back to the guest kernel's mount-time replay"
                );
            }
        })
        .await;
}

pub fn recovery_step(fs_quiesced: bool, requested: bool, record: &RecoveryRecord) -> Option<PreBoot> {
    let trigger = match requested {
        true => TRIGGER_RESCUE,
        false => TRIGGER_LEGACY,
    };
    if fs_quiesced {
        record.set(RecoverOutcome::SkippedQuiesced);
        FsRecoveryRun {
            result: RecoverOutcome::SkippedQuiesced.label(),
            reason: RecoverReason::Quiesced.label(),
            trigger,
        }
        .record();
        return None;
    }
    let record = record.clone();
    Some(Box::new(
        move |rootfs: PathBuf| -> BoxFuture<'static, Result<(), String>> {
            let span = info_span!(
                "fs-recover",
                "fs_recovery.trigger" = trigger,
                "fs_recovery.outcome" = Empty,
                "fs_recovery.reason" = Empty
            );
            Box::pin(recover(rootfs, record, trigger).instrument(span))
        },
    ))
}

async fn recover(rootfs: PathBuf, record: RecoveryRecord, trigger: &'static str) -> Result<(), String> {
    let started = Instant::now();
    let recovery = recover_filesystem(&rootfs.to_string_lossy()).await;
    let span = tracing::Span::current();
    span.record("fs_recovery.outcome", recovery.outcome.label());
    span.record("fs_recovery.reason", recovery.reason.label());
    record.set(recovery.outcome);
    FsRecoveryRun {
        result: recovery.outcome.label(),
        reason: recovery.reason.label(),
        trigger,
    }
    .record_with_duration(started.elapsed());
    match (recovery.error, recovery.outcome) {
        (Some(error), _) => Err(error),
        (None, RecoverOutcome::FailedOpen) => {
            tracing::warn!(
                reason = recovery.reason.label(),
                "pre-boot filesystem recovery could not run; booting on the guest kernel's mount-time replay"
            );
            Ok(())
        }
        (None, outcome) => {
            tracing::info!(
                outcome = outcome.label(),
                duration = ?started.elapsed(),
                "preboot filesystem recovery finished"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiesced_snapshot_records_the_skip_and_needs_no_step() {
        let record = RecoveryRecord::default();

        assert!(recovery_step(true, false, &record).is_none());
        assert_eq!(record.get(), RecoverOutcome::SkippedQuiesced);
    }

    #[test]
    fn a_snapshot_that_was_not_frozen_gets_a_recovery_step_for_both_triggers() {
        for requested in [true, false] {
            let record = RecoveryRecord::default();

            assert!(recovery_step(false, requested, &record).is_some());
            assert_eq!(record.get(), RecoverOutcome::None);
        }
    }

    #[tokio::test]
    async fn a_failed_recovery_fails_the_boot_and_is_recorded() {
        let record = RecoveryRecord::default();
        let step = recovery_step(false, true, &record).unwrap();

        let error = step(PathBuf::from("/dev/sda")).await.unwrap_err();

        assert!(
            error.starts_with("rootfs filesystem recovery did not complete"),
            "{error}"
        );
        assert_eq!(record.get(), RecoverOutcome::Failed);
    }
}
