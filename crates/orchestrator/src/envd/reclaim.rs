use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use super::quiesce::guest::{EnvdFailure, EnvdReply, GuestEnvd};
use super::version::is_at_least;
use crate::featureflags::{
    COLLAPSE_ENVD_HEAP, COLLAPSE_ENVD_HEAP_TIMEOUT_MS, FREEZE_GUEST_HIERARCHY, FREEZE_GUEST_HIERARCHY_MAX_CGROUPS,
    FREEZE_USER_CGROUP_TIMEOUT_MS, FeatureFlags, FlagContext, GUEST_PAUSE_RECLAIM, IntFlag, freeze_user_cgroup,
};
use crate::go_strconv::quote;
use crate::telemetry::reclaim::{
    FreezeCall, FreezeObserved, record_collapse_chunks, record_collapse_duration, record_freeze_call,
    record_freeze_observed, record_unfreeze,
};

pub const MIN_FOR_CGROUP_FREEZE: &str = "0.6.3";
pub const MIN_FOR_HEAP_COLLAPSE: &str = "0.6.5";
const FREEZE_TIMEOUT: Duration = Duration::from_secs(2);
const FREEZE_ROUND_TRIP_MARGIN: Duration = Duration::from_millis(500);
const OUTER_SLACK: Duration = Duration::from_millis(500);
const LEGACY_MODE: &str = "legacy";
const HIERARCHY_MODE: &str = "hierarchy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreezeSettings {
    pub timeout: Duration,
    pub hierarchy: bool,
    pub max_cgroups: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReclaimSteps {
    pub fstrim: Duration,
    pub sync: Duration,
    pub drop_caches: Duration,
    pub compact_memory: Duration,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimSettings {
    pub freeze: Option<FreezeSettings>,
    pub collapse_timeout: Option<Duration>,
    pub steps: ReclaimSteps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimScript {
    pub script: String,
    pub timeout: Duration,
}

impl ReclaimSettings {
    pub fn resolve(flags: &FeatureFlags, contexts: &[FlagContext]) -> Self {
        let millis = |flag: &IntFlag| Duration::from_millis(flags.get(flag, contexts).max(0) as u64);
        let freeze = flags.get(&freeze_user_cgroup(), contexts).then(|| FreezeSettings {
            timeout: Some(millis(&FREEZE_USER_CGROUP_TIMEOUT_MS))
                .filter(|timeout| !timeout.is_zero())
                .unwrap_or(FREEZE_TIMEOUT),
            hierarchy: flags.get(&FREEZE_GUEST_HIERARCHY, contexts),
            max_cgroups: flags.get(&FREEZE_GUEST_HIERARCHY_MAX_CGROUPS, contexts),
        });
        Self {
            freeze,
            collapse_timeout: flags
                .get(&COLLAPSE_ENVD_HEAP, contexts)
                .then(|| millis(&COLLAPSE_ENVD_HEAP_TIMEOUT_MS)),
            steps: ReclaimSteps::from_flag(&flags.get(&GUEST_PAUSE_RECLAIM, contexts)),
        }
    }
}

impl ReclaimSteps {
    fn from_flag(config: &Value) -> Self {
        let millis =
            |key: &str| Duration::from_millis(config.get(key).and_then(Value::as_i64).unwrap_or(0).max(0) as u64);
        Self {
            fstrim: millis("fstrim"),
            sync: millis("sync"),
            drop_caches: millis("drop_caches"),
            compact_memory: millis("compact_memory"),
        }
    }

    pub fn script(&self) -> Option<ReclaimScript> {
        let steps = [
            (self.fstrim, "fstrim -av"),
            (self.sync, "sync"),
            (self.drop_caches, "echo 3 > /proc/sys/vm/drop_caches"),
            (self.compact_memory, "echo 1 > /proc/sys/vm/compact_memory"),
        ];
        let enabled: Vec<(Duration, &str)> = steps
            .into_iter()
            .filter(|(cap, _)| *cap >= Duration::from_millis(1))
            .collect();
        (!enabled.is_empty()).then(|| ReclaimScript {
            script: format!(
                "rc=0; {}; exit $rc",
                enabled
                    .iter()
                    .map(|(cap, command)| format!(
                        "timeout -s KILL {:.3} sh -c {} >/dev/null 2>&1 || rc=$?",
                        cap.as_secs_f64(),
                        quote(command)
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            timeout: enabled.iter().map(|(cap, _)| *cap).sum::<Duration>() + OUTER_SLACK,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FreezeReport {
    pub allowlisted: i64,
    pub failed: i64,
    pub frozen: i64,
    pub mode: String,
    pub not_frozen: i64,
    pub pre_frozen: i64,
    pub requested: i64,
    pub sweep_ms: i64,
    pub truncated: bool,
    pub unobservable: i64,
    pub vanished: i64,
    pub visited: i64,
    pub wait_ms: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CollapseReport {
    pub regions: u64,
    pub chunks: u64,
    pub collapsed: u64,
    pub already_huge: u64,
    pub skipped: u64,
}

pub struct GuestReclaim<'a> {
    pub envd: GuestEnvd,
    pub envd_version: &'a str,
    pub sandbox_id: &'a str,
    pub team_id: &'a str,
}

impl GuestReclaim<'_> {
    pub async fn before_pause(&self, settings: &ReclaimSettings, skip_heap_collapse: bool) {
        if let Some(freeze) = settings.freeze {
            self.freeze(freeze).await;
        }
        if let Some(timeout) = settings.collapse_timeout.filter(|_| !skip_heap_collapse) {
            self.collapse(timeout).await;
        }
        if let Some(script) = settings.steps.script() {
            self.run_script(&script).await;
        }
    }

    pub async fn unfreeze(&self) {
        if !self.supports(MIN_FOR_CGROUP_FREEZE, "cgroup freeze") {
            return;
        }
        let started = Instant::now();
        let unfrozen = self.envd.post("unfreeze", FREEZE_TIMEOUT).await;
        record_unfreeze(unfrozen.is_ok(), started.elapsed(), self.team_id);
        if let Err(e) = unfrozen {
            tracing::warn!(sandbox.id = self.sandbox_id, error = %e, "envd unfreeze failed");
        }
    }

    fn supports(&self, minimum: &str, feature: &str) -> bool {
        is_at_least(self.envd_version, minimum).unwrap_or_else(|e| {
            tracing::warn!(
                sandbox.id = self.sandbox_id,
                envd_version = self.envd_version,
                error = %e,
                "{feature} version gate: bad envd version"
            );
            false
        })
    }

    async fn freeze(&self, settings: FreezeSettings) {
        if !self.supports(MIN_FOR_CGROUP_FREEZE, "cgroup freeze") {
            return;
        }
        let started = Instant::now();
        let reply = self
            .envd
            .post_for_body("freeze", &freeze_query(settings), settings.timeout)
            .await;
        let took = started.elapsed();
        let freeze = reply
            .map_err(|failure| (failure.timed_out, failure.message))
            .and_then(|reply| freeze_report(&reply).map_err(|message| (false, message)));
        let call = FreezeCall {
            success: freeze.is_ok(),
            reported: freeze.as_ref().is_ok_and(Option::is_some),
            timed_out: freeze.as_ref().err().is_some_and(|(timed_out, _)| *timed_out),
            timeout: settings.timeout,
        };
        record_freeze_call(call, took, self.team_id);
        match freeze {
            Err((_, e)) => tracing::warn!(sandbox.id = self.sandbox_id, error = %e, "envd freeze failed"),
            Ok(None) => {}
            Ok(Some(report)) => self.report_freeze(&report, settings),
        }
    }

    fn report_freeze(&self, report: &FreezeReport, settings: FreezeSettings) {
        let mode = match report.mode.as_str() {
            "" => LEGACY_MODE,
            mode => mode,
        };
        record_freeze_observed(&FreezeObserved {
            mode,
            sweep_ms: report.sweep_ms,
            wait_ms: report.wait_ms,
            visited: report.visited,
            outcomes: [
                ("frozen", report.frozen),
                ("not_frozen", report.not_frozen),
                ("failed", report.failed),
                ("unobservable", report.unobservable),
                ("pre_frozen", report.pre_frozen),
                ("vanished", report.vanished),
            ],
        });
        if report.truncated {
            tracing::warn!(
                sandbox.id = self.sandbox_id,
                visited = report.visited,
                max_cgroups = settings.max_cgroups,
                requested = report.requested,
                "pre-pause freeze walk hit its bound; coverage is incomplete"
            );
        }
        if report.not_frozen > 0 || report.failed > 0 {
            tracing::warn!(
                sandbox.id = self.sandbox_id,
                requested = report.requested,
                frozen = report.frozen,
                not_frozen = report.not_frozen,
                failed = report.failed,
                vanished = report.vanished,
                wait_ms = report.wait_ms,
                "pre-pause freeze did not stop the whole workload"
            );
        }
    }

    async fn collapse(&self, timeout: Duration) {
        if !self.supports(MIN_FOR_HEAP_COLLAPSE, "heap collapse") {
            return;
        }
        let started = Instant::now();
        let collapsed = self
            .envd
            .post_for_body("collapse", "", timeout)
            .await
            .map_err(|failure: EnvdFailure| failure.message)
            .and_then(|reply| collapse_report(&reply));
        let took = started.elapsed();
        record_collapse_duration(collapsed.is_ok(), took, self.team_id);
        match collapsed {
            Err(e) => tracing::warn!(sandbox.id = self.sandbox_id, error = %e, "envd heap collapse failed"),
            Ok(report) => {
                record_collapse_chunks(report.collapsed, report.already_huge, report.skipped);
                tracing::info!(
                    sandbox.id = self.sandbox_id,
                    regions = report.regions,
                    chunks = report.chunks,
                    collapsed = report.collapsed,
                    already_huge = report.already_huge,
                    skipped = report.skipped,
                    duration_ms = took.as_millis() as u64,
                    "envd heap collapsed"
                );
            }
        }
    }

    async fn run_script(&self, script: &ReclaimScript) {
        match self.envd.shell(&script.script, script.timeout).await {
            Err(e) => tracing::warn!(sandbox.id = self.sandbox_id, error = %e, "envd reclaim failed"),
            Ok(0) => {}
            Ok(exit_code) => {
                tracing::warn!(sandbox.id = self.sandbox_id, exit_code, "envd reclaim non-zero exit")
            }
        }
    }
}

fn freeze_query(settings: FreezeSettings) -> String {
    let max_wait_ms = (settings.timeout.saturating_sub(FREEZE_ROUND_TRIP_MARGIN).as_millis() as i64).max(1);
    match settings.hierarchy {
        true => format!(
            "maxWaitMs={max_wait_ms}&mode={HIERARCHY_MODE}&maxCgroups={}",
            settings.max_cgroups
        ),
        false => format!("maxWaitMs={max_wait_ms}"),
    }
}

fn freeze_report(reply: &EnvdReply) -> Result<Option<FreezeReport>, String> {
    match reply.status {
        204 => Ok(None),
        200 => serde_json::from_slice(&reply.body)
            .map(Some)
            .map_err(|e| format!("decode freeze result: {e}")),
        _ => Err(GuestEnvd::error_text("freeze", reply)),
    }
}

fn collapse_report(reply: &EnvdReply) -> Result<CollapseReport, String> {
    match reply.status {
        200 => serde_json::from_slice(&reply.body).map_err(|e| format!("decode collapse result: {e}")),
        _ => Err(GuestEnvd::error_text("collapse", reply)),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::json;

    use super::*;

    fn steps(fstrim: u64, sync: u64, drop_caches: u64, compact_memory: u64) -> ReclaimSteps {
        ReclaimSteps {
            fstrim: Duration::from_millis(fstrim),
            sync: Duration::from_millis(sync),
            drop_caches: Duration::from_millis(drop_caches),
            compact_memory: Duration::from_millis(compact_memory),
        }
    }

    #[test]
    fn every_step_disabled_means_no_script() {
        assert_eq!(ReclaimSteps::default().script(), None);
    }

    #[test]
    fn the_script_chains_the_enabled_steps_like_go() {
        assert_eq!(
            steps(0, 1500, 0, 250).script(),
            Some(ReclaimScript {
                script: "rc=0; timeout -s KILL 1.500 sh -c \"sync\" >/dev/null 2>&1 || rc=$?; timeout -s KILL 0.250 sh -c \"echo 1 > /proc/sys/vm/compact_memory\" >/dev/null 2>&1 || rc=$?; exit $rc".to_owned(),
                timeout: Duration::from_millis(2250),
            })
        );
    }

    #[test]
    fn the_reclaim_flag_reads_milliseconds_per_step() {
        let config = json!({"fstrim": 3000, "sync": 1000, "drop_caches": "x", "compact_memory": -5});

        assert_eq!(ReclaimSteps::from_flag(&config), steps(3000, 1000, 0, 0));
        assert_eq!(ReclaimSteps::from_flag(&Value::Null), ReclaimSteps::default());
    }

    fn flags(overrides: &[(&str, Value)]) -> FeatureFlags {
        overrides
            .iter()
            .fold(FeatureFlags::builder(), |builder, (key, value)| {
                builder.override_flag(key, value.clone())
            })
            .offline()
    }

    #[test]
    fn unset_flags_reclaim_nothing_outside_development() {
        let settings = ReclaimSettings::resolve(&flags(&[("freeze-user-cgroup", json!(false))]), &[]);

        assert_eq!(settings, ReclaimSettings::default());
    }

    #[test]
    fn the_freeze_flags_shape_the_request() {
        let settings = ReclaimSettings::resolve(
            &flags(&[
                ("freeze-user-cgroup", json!(true)),
                ("freeze-user-cgroup-timeout-ms", json!(0)),
                ("freeze-guest-hierarchy", json!(true)),
                ("collapse-envd-heap", json!(true)),
            ]),
            &[],
        );
        let freeze = settings.freeze.unwrap();

        assert_eq!(freeze.timeout, FREEZE_TIMEOUT);
        assert_eq!(freeze_query(freeze), "maxWaitMs=1500&mode=hierarchy&maxCgroups=512");
        assert_eq!(settings.collapse_timeout, Some(Duration::from_secs(10)));
        assert_eq!(
            freeze_query(FreezeSettings {
                timeout: Duration::from_millis(300),
                hierarchy: false,
                max_cgroups: 512
            }),
            "maxWaitMs=1"
        );
    }

    fn reply(status: u16, body: &'static str) -> EnvdReply {
        EnvdReply {
            status,
            body: Bytes::from_static(body.as_bytes()),
        }
    }

    #[test]
    fn a_freeze_reply_is_a_report_a_silent_success_or_an_error() {
        assert_eq!(freeze_report(&reply(204, "")), Ok(None));
        assert_eq!(
            freeze_report(&reply(
                200,
                r#"{"frozen":3,"notFrozen":1,"mode":"hierarchy","waitMs":12}"#
            )),
            Ok(Some(FreezeReport {
                frozen: 3,
                not_frozen: 1,
                mode: "hierarchy".to_owned(),
                wait_ms: 12,
                ..FreezeReport::default()
            }))
        );
        assert_eq!(
            freeze_report(&reply(500, r#"{"message":"boom"}"#)),
            Err(r#"freeze returned 500: {"message":"boom"}"#.to_owned())
        );
    }

    #[test]
    fn a_collapse_reply_carries_the_chunk_counts() {
        assert_eq!(
            collapse_report(&reply(
                200,
                r#"{"alreadyHuge":2,"chunks":5,"collapsed":3,"elapsedMs":1,"regions":1,"skipped":0}"#
            )),
            Ok(CollapseReport {
                regions: 1,
                chunks: 5,
                collapsed: 3,
                already_huge: 2,
                skipped: 0
            })
        );
        assert!(
            collapse_report(&reply(404, ""))
                .unwrap_err()
                .starts_with("collapse returned 404")
        );
    }
}
