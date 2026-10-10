use std::path::{Path, PathBuf};
use std::time::Instant;

use futures::future::BoxFuture;
use tracing::{Instrument, info_span};

use crate::envd::binary_cache::resolver::{REASON_COPY_VANISHED, REASON_NOT_CACHED, source_path};
use crate::envd::upgrade::rollout::{EnvdUpgrades, OfflineTarget};
use crate::featureflags::FlagContext;
use crate::sandbox::config::PreBoot;
use crate::sandbox::rootfs::envd_swap::{SwapError, Swapped, swap_envd_binary};
use crate::telemetry::reboot::OfflineUpgradeAttempt;

const NOT_QUIESCED: &str = "not_quiesced";
const SOURCE_STALLED: &str = "source_stalled";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfflineSwapDecision {
    pub swap: bool,
    pub count_result: Option<String>,
    pub log_misconfig: bool,
}

pub fn decide_offline_swap(target: Option<&Path>, reason: &str, fs_quiesced: bool) -> OfflineSwapDecision {
    let counted = |log_misconfig: bool| OfflineSwapDecision {
        swap: false,
        count_result: Some(reason.to_owned()),
        log_misconfig,
    };
    match (target, reason) {
        (None, "" | "off") => OfflineSwapDecision::default(),
        (None, "same_version" | REASON_NOT_CACHED | SOURCE_STALLED) => counted(false),
        (None, _) => counted(true),
        (Some(_), _) if !fs_quiesced => OfflineSwapDecision {
            swap: false,
            count_result: Some(NOT_QUIESCED.to_owned()),
            log_misconfig: false,
        },
        (Some(_), _) => OfflineSwapDecision {
            swap: true,
            ..OfflineSwapDecision::default()
        },
    }
}

pub struct OfflineUpgrade<'a> {
    pub upgrades: &'a EnvdUpgrades,
    pub sandbox_id: &'a str,
    pub template_id: &'a str,
    pub from_version: &'a str,
    pub fs_quiesced: bool,
}

impl OfflineUpgrade<'_> {
    pub async fn pre_boot(self) -> Option<PreBoot> {
        let contexts = [
            FlagContext::sandbox(self.sandbox_id),
            FlagContext::template(self.template_id),
        ];
        let span = info_span!("resolve offline envd upgrade", "envd.from_version" = self.from_version);
        let target = self
            .upgrades
            .offline_target(&contexts, self.from_version)
            .instrument(span)
            .await;
        let decision = decide_offline_swap(target.path.as_deref(), &target.reason, self.fs_quiesced);
        match decision.swap {
            true => Some(self.swap_step(target)),
            false => {
                self.report_no_swap(&target, &decision);
                None
            }
        }
    }

    fn report_no_swap(&self, target: &OfflineTarget, decision: &OfflineSwapDecision) {
        if decision.log_misconfig {
            tracing::info!(
                sandbox.id = self.sandbox_id,
                reason = %target.reason,
                built_with = self.from_version,
                "offline envd upgrade: target not resolved"
            );
        }
        if decision.count_result.as_deref() == Some(NOT_QUIESCED) {
            tracing::info!(
                sandbox.id = self.sandbox_id,
                built_with = self.from_version,
                target = ?target.path,
                to_version = %target.to_version,
                "skipping offline envd upgrade: snapshot rootfs was not frozen at pause (not crash-consistent); awaiting a future freezing pause"
            );
        }
        if let Some(result) = &decision.count_result {
            OfflineUpgradeAttempt {
                result,
                from_version: self.from_version,
                to_version: &target.to_version,
                refire: None,
            }
            .record();
        }
    }

    fn swap_step(&self, target: OfflineTarget) -> PreBoot {
        let swap = OfflineSwap {
            sandbox_id: self.sandbox_id.to_owned(),
            from_version: self.from_version.to_owned(),
            stage_root: self.upgrades.stage_root().to_path_buf(),
            target,
        };
        Box::new(move |rootfs: PathBuf| -> BoxFuture<'static, Result<(), String>> {
            Box::pin(async move { swap.run(rootfs).await })
        })
    }
}

struct OfflineSwap {
    sandbox_id: String,
    from_version: String,
    stage_root: PathBuf,
    target: OfflineTarget,
}

impl OfflineSwap {
    async fn run(self, rootfs: PathBuf) -> Result<(), String> {
        let started = Instant::now();
        let Some(path) = self.target.path.clone() else {
            return Ok(());
        };
        let Some(source) = source_path(self.target.resolver.as_ref(), &path).await else {
            tracing::warn!(
                sandbox.id = %self.sandbox_id,
                target = %path.display(),
                built_with = %self.from_version,
                "skipping offline envd upgrade: cached binary went away after its version was resolved"
            );
            self.attempt(REASON_COPY_VANISHED, None).record();
            return Ok(());
        };
        let swapped = swap_envd_binary(&rootfs.to_string_lossy(), &source, &self.stage_root).await;
        let result = self.report(&path, &swapped);
        let refire = swapped.error.is_none().then_some(swapped.refire);
        self.attempt(result, refire).record_with_duration(started.elapsed());
        match swapped.error {
            Some(SwapError::Unrecoverable(message)) => {
                Err(format!("offline envd upgrade left rootfs unbootable: {message}"))
            }
            _ => Ok(()),
        }
    }

    fn attempt<'a>(&'a self, result: &'a str, refire: Option<bool>) -> OfflineUpgradeAttempt<'a> {
        OfflineUpgradeAttempt {
            result,
            from_version: &self.from_version,
            to_version: &self.target.to_version,
            refire,
        }
    }

    fn report(&self, path: &Path, swapped: &Swapped) -> &'static str {
        let (target, built_with, sandbox) = (path.display(), &self.from_version, &self.sandbox_id);
        let cached = self.target.resolver.is_some();
        match &swapped.error {
            None => {
                tracing::info!(sandbox.id = %sandbox, %target, to_version = %self.target.to_version, %built_with, refire = swapped.refire, "swapped envd binary before reboot");
                "success"
            }
            Some(SwapError::TooLarge(e)) => {
                tracing::warn!(sandbox.id = %sandbox, %target, %built_with, error = %e, "skipping offline envd upgrade: rootfs envd is too large to swap");
                "envd_too_large"
            }
            Some(SwapError::Missing(e)) => {
                tracing::warn!(sandbox.id = %sandbox, %target, %built_with, error = %e, "skipping offline envd upgrade: rootfs has no envd to swap");
                "envd_missing"
            }
            Some(SwapError::StatUnparseable(e)) => {
                tracing::warn!(sandbox.id = %sandbox, %target, %built_with, error = %e, "skipping offline envd upgrade: rootfs envd stat was unreadable");
                "stat_unparseable"
            }
            Some(SwapError::Unrecoverable(e)) => {
                tracing::error!(sandbox.id = %sandbox, %target, %built_with, error = %e, "offline envd swap left the rootfs without a usable envd; failing boot to discard the overlay");
                "unrecoverable"
            }
            Some(SwapError::SourceMissing(e)) if cached => {
                tracing::warn!(sandbox.id = %sandbox, %target, %built_with, error = %e, "skipping offline envd upgrade: cached binary went away before the swap read it");
                REASON_COPY_VANISHED
            }
            Some(SwapError::SourceMissing(e) | SwapError::Failed(e)) => {
                tracing::error!(sandbox.id = %sandbox, %target, %built_with, error = %e, "offline envd swap before reboot failed; booting original envd");
                "swap_failed"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        target: bool,
        reason: &'static str,
        fs_quiesced: bool,
        swap: bool,
        count: Option<&'static str>,
        log: bool,
    }

    fn case(name: &'static str, reason: &'static str, count: Option<&'static str>, log: bool) -> Case {
        Case {
            name,
            target: false,
            reason,
            fs_quiesced: true,
            swap: false,
            count,
            log,
        }
    }

    #[test]
    fn the_offline_swap_is_gated_like_go() {
        let cases = [
            case("off", "off", None, false),
            case("empty reason", "", None, false),
            case("same_version", "same_version", Some("same_version"), false),
            case("not_staged", "not_staged", Some("not_staged"), true),
            case("downgrade", "downgrade", Some("downgrade"), true),
            case("invalid_target", "invalid_target", Some("invalid_target"), true),
            case(
                "getversion_failed",
                "getversion_failed",
                Some("getversion_failed"),
                true,
            ),
            case("not cached", REASON_NOT_CACHED, Some(REASON_NOT_CACHED), false),
            Case {
                fs_quiesced: false,
                ..case(
                    "not cached, not frozen",
                    REASON_NOT_CACHED,
                    Some(REASON_NOT_CACHED),
                    false,
                )
            },
            case("stalled", SOURCE_STALLED, Some(SOURCE_STALLED), false),
            Case {
                fs_quiesced: false,
                ..case("same_version, not frozen", "same_version", Some("same_version"), false)
            },
            case(
                "unknown reason",
                "unresolvable_frobnicator",
                Some("unresolvable_frobnicator"),
                true,
            ),
            Case {
                target: true,
                fs_quiesced: false,
                ..case("target, not frozen", "", Some(NOT_QUIESCED), false)
            },
            Case {
                target: true,
                swap: true,
                ..case("target and frozen", "", None, false)
            },
            Case {
                fs_quiesced: false,
                ..case("off, not frozen", "off", None, false)
            },
        ];
        let target = Path::new("/fc-envd/envd.abc123");
        for case in cases {
            let decision = decide_offline_swap(case.target.then_some(target), case.reason, case.fs_quiesced);

            assert_eq!(
                decision,
                OfflineSwapDecision {
                    swap: case.swap,
                    count_result: case.count.map(str::to_owned),
                    log_misconfig: case.log
                },
                "{}",
                case.name
            );
        }
    }
}
