use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures::FutureExt;
use tokio::time::Instant;
use tracing::Instrument;

use super::{Node, SANDBOX_TYPE};
use crate::envd::binary_cache::resolver::REASON_COPY_VANISHED;
use crate::envd::report::HandoverResult;
use crate::envd::upgrade::phase::{DELIVER, READY, RESOLVE, delivery_result, phase_result};
use crate::envd::upgrade::rollout::{Delivery, PhaseSample, Plan, Skipped, Subject};
use crate::featureflags::FlagContext;
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;
use crate::telemetry::report::record_exception;
use crate::telemetry::server::envd_upgrade::{HandoverItem, UpgradeAttempt, record_gated, record_phase};
use crate::telemetry::server::spans::envd_upgrade;

const GUEST_BINARY_PATH: &str = "/usr/bin/envd.next";
const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(15);

struct Attempt {
    team_id: String,
    from_version: String,
    to_version: String,
    result: &'static str,
    attempted: bool,
    started: Instant,
}

impl Attempt {
    fn new(team_id: &str, from_version: String) -> Self {
        Self {
            team_id: team_id.to_owned(),
            from_version,
            to_version: String::new(),
            result: "success",
            attempted: false,
            started: Instant::now(),
        }
    }

    fn begin(&mut self, to_version: &str) {
        self.to_version = to_version.to_owned();
        self.attempted = true;
        self.started = Instant::now();
    }

    fn finish(&self) {
        if self.attempted {
            UpgradeAttempt {
                result: self.result.to_owned(),
                from_version: self.from_version.clone(),
                to_version: self.to_version.clone(),
                team_id: self.team_id.clone(),
            }
            .record(self.started.elapsed());
        }
    }
}

impl Node {
    pub(super) async fn maybe_upgrade_envd(
        &self,
        sandbox: &mut Sandbox,
        config: &SandboxConfig,
    ) -> Result<bool, String> {
        let from_version = Some(sandbox.live_envd_version())
            .filter(|version| !version.is_empty())
            .unwrap_or_else(|| sandbox.built_with_envd_version().to_owned());
        let mut attempt = Attempt::new(&config.team_id, from_version);
        let outcome = AssertUnwindSafe(self.upgrade(sandbox, config, &mut attempt))
            .catch_unwind()
            .await;
        let upgraded = outcome.unwrap_or_else(|_| {
            tracing::error!(sandbox = %config.sandbox_id, "envd auto-upgrade panic (recovered)");
            attempt.result = "panic";
            attempt.attempted = true;
            Ok(false)
        });
        attempt.finish();
        upgraded
    }

    async fn upgrade(
        &self,
        sandbox: &mut Sandbox,
        config: &SandboxConfig,
        attempt: &mut Attempt,
    ) -> Result<bool, String> {
        let subject = Subject {
            contexts: flag_contexts(config),
            from_version: attempt.from_version.clone(),
            sent_user: sandbox.sent_default_user(),
            workdir_withheld: sandbox.workdir_withheld(),
            reported_defaults: sandbox.reported_defaults(),
        };
        match self.envd_upgrades.plan(&subject).await {
            Plan::Skip(skipped) => {
                record_skip(&attempt.team_id, &attempt.from_version, skipped);
                Ok(false)
            }
            Plan::Deliver(delivery) => {
                attempt.begin(&delivery.to_version);
                record_sample(&attempt.team_id, RESOLVE, delivery.resolve.clone());
                let span = envd_upgrade(&attempt.from_version, &delivery.to_version);
                self.deliver(sandbox, config, attempt, delivery).instrument(span).await
            }
        }
    }

    async fn deliver(
        &self,
        sandbox: &mut Sandbox,
        config: &SandboxConfig,
        attempt: &mut Attempt,
        delivery: Delivery,
    ) -> Result<bool, String> {
        tracing::info!(
            sandbox = %config.sandbox_id,
            from = %attempt.from_version,
            to = %delivery.to_version,
            path = %delivery.source.display(),
            bin_cache = delivery.cache_outcome.map_or("", |outcome| outcome.as_str()),
            "envd auto-upgrade: delivering+triggering"
        );
        let started = Instant::now();
        let delivered = sandbox
            .call_envd_upgrade(&delivery.deliver_from, GUEST_BINARY_PATH, DELIVER_TIMEOUT)
            .await;
        let copy_gone = delivery.cached && delivered.as_ref().is_err_and(|failure| failure.source_missing);
        if !copy_gone {
            record_phase(
                &attempt.team_id,
                DELIVER,
                delivery_result(&delivered),
                started.elapsed(),
            );
        }
        let confirmed = match delivered {
            Ok(confirmed) => confirmed,
            Err(_) if copy_gone => {
                attempt.attempted = false;
                record_gated(REASON_COPY_VANISHED);
                tracing::warn!(
                    sandbox = %config.sandbox_id,
                    from = %attempt.from_version,
                    to = %delivery.to_version,
                    path = %delivery.source.display(),
                    "envd auto-upgrade: cached binary went away before delivery; deferring"
                );
                return Ok(false);
            }
            Err(failure) => {
                attempt.result = "delivery_failed";
                record_exception(&failure.message);
                tracing::error!(sandbox = %config.sandbox_id, error = %failure, "envd auto-upgrade: trigger failed");
                return Ok(false);
            }
        };
        self.confirm(sandbox, config, attempt, &delivery, confirmed).await
    }

    async fn confirm(
        &self,
        sandbox: &mut Sandbox,
        config: &SandboxConfig,
        attempt: &mut Attempt,
        delivery: &Delivery,
        exec_confirmed: bool,
    ) -> Result<bool, String> {
        let started = Instant::now();
        let ready = sandbox.wait_for_envd_within(READY_TIMEOUT).await;
        record_phase(&attempt.team_id, READY, phase_result(&ready), started.elapsed());
        if let Err(error) = ready {
            attempt.result = "not_ready";
            record_exception(&error);
            tracing::error!(sandbox = %config.sandbox_id, error = %error, "envd auto-upgrade: envd not ready after upgrade");
            return match exec_confirmed {
                false => Ok(false),
                true => Err(format!(
                    "envd live-upgrade left the sandbox uninitialized (post-upgrade /init failed): {error}"
                )),
            };
        }
        let observed = sandbox.live_envd_version();
        if observed != delivery.to_version {
            attempt.result = "version_mismatch";
            tracing::Span::current().record("envd.observed_version", observed.as_str());
            tracing::error!(
                sandbox = %config.sandbox_id,
                observed = %observed,
                expected = %delivery.to_version,
                "envd auto-upgrade: version did not flip"
            );
            return Ok(false);
        }
        let handover = sandbox.handover_result();
        if handover.as_ref().is_some_and(|handover| handover.failed) {
            attempt.result = "handover_failed";
            tracing::Span::current().record("envd.handover.failed", true);
            tracing::error!(
                sandbox = %config.sandbox_id,
                "envd auto-upgrade: in-guest handover failed post-exec; workload not re-adopted"
            );
            return Err("envd live-upgrade handover failed post-exec (workload not re-adopted)".to_owned());
        }
        tracing::Span::current().record("envd.upgraded", true);
        record_handover(&config.sandbox_id, &delivery.to_version, handover.as_ref());
        Ok(true)
    }
}

fn flag_contexts(config: &SandboxConfig) -> Vec<FlagContext> {
    vec![
        FlagContext::sandbox_runtime(
            &config.sandbox_id,
            &config.template_id,
            &config.kernel_version,
            &config.firecracker_version,
            &config.envd_version,
            SANDBOX_TYPE,
        ),
        FlagContext::team(&config.team_id),
    ]
}

fn record_sample(team_id: &str, phase: &str, sample: Option<PhaseSample>) {
    sample
        .iter()
        .for_each(|sample| record_phase(team_id, phase, &sample.result, sample.took));
}

fn record_skip(team_id: &str, from_version: &str, skipped: Skipped) {
    record_sample(team_id, RESOLVE, skipped.resolve);
    skipped.gated.iter().for_each(|reason| record_gated(reason));
    skipped.warn.iter().for_each(
        |reason| tracing::warn!(reason = %reason, from = %from_version, "envd auto-upgrade: target not resolved"),
    );
}

fn record_handover(sandbox_id: &str, to_version: &str, handover: Option<&HandoverResult>) {
    let Some(handover) = handover else {
        tracing::info!(sandbox = %sandbox_id, to = %to_version, "envd auto-upgrade: complete");
        return;
    };
    let items = [
        ("proc", handover.procs, handover.procs_failed),
        ("retained", handover.retained, handover.retained_failed),
        ("watcher", handover.watchers, handover.watchers_failed),
    ];
    for (name, total, failed) in items {
        HandoverItem {
            name,
            total: total.max(0) as u64,
            failed: failed.max(0) as u64,
        }
        .record();
    }
    let span = tracing::Span::current();
    span.record("envd.handover.procs", handover.procs);
    span.record("envd.handover.procs_failed", handover.procs_failed);
    span.record("envd.handover.retained", handover.retained);
    span.record("envd.handover.retained_failed", handover.retained_failed);
    span.record("envd.handover.watchers", handover.watchers);
    span.record("envd.handover.watchers_failed", handover.watchers_failed);
    tracing::info!(
        sandbox = %sandbox_id,
        to = %to_version,
        procs = handover.procs,
        procs_failed = handover.procs_failed,
        retained = handover.retained,
        retained_failed = handover.retained_failed,
        watchers = handover.watchers,
        watchers_failed = handover.watchers_failed,
        "envd auto-upgrade: complete"
    );
}
