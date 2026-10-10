use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use crate::firecracker::client::{
    BalloonCaps, FirecrackerClient, HintingCallError, HintingStart, HintingStatus, HintingStop,
};
use crate::firecracker::metrics::FirecrackerMetrics;
use crate::telemetry::hinting::{record_hint_freed, record_hint_run, record_hint_stop};

const HINT_STOP: i64 = 0;
const HINT_DONE: i64 = 1;
const FIRST_HINT_CMD: i64 = 2;
const CONFIG_READ_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_STOP_GRACE: Duration = Duration::from_millis(100);
const FIRST_BACKOFF: Duration = Duration::from_millis(5);
const MAX_BACKOFF: Duration = Duration::from_millis(50);
const EXIT_CHECKS: usize = 5;
const EXIT_CHECK_INTERVAL: Duration = Duration::from_millis(20);
const PHASE_PRE_PAUSE: &str = "pre-pause";
const FREED_READ_TIMEOUT: Duration = Duration::from_millis(500);
pub const USE_CASE_PAUSE: &str = "pause";
pub const USE_CASE_BUILD: &str = "build";

pub trait HintingApi: Sync {
    fn caps(&self) -> impl Future<Output = Result<BalloonCaps, String>> + Send;
    fn start(&self) -> impl Future<Output = Result<HintingStart, HintingCallError>> + Send;
    fn stop(&self) -> impl Future<Output = Result<HintingStop, HintingCallError>> + Send;
    fn status(&self) -> impl Future<Output = Result<HintingStatus, HintingCallError>> + Send;
    fn hint_freed(&self) -> u64;
    fn flush_hint_freed(&self) -> impl Future<Output = Result<u64, String>> + Send;
}

pub struct VmHinting<'a> {
    pub client: FirecrackerClient,
    pub metrics: Option<&'a FirecrackerMetrics>,
}

impl HintingApi for VmHinting<'_> {
    async fn caps(&self) -> Result<BalloonCaps, String> {
        self.client.balloon_caps().await
    }

    async fn start(&self) -> Result<HintingStart, HintingCallError> {
        self.client.start_balloon_hinting().await
    }

    async fn stop(&self) -> Result<HintingStop, HintingCallError> {
        self.client.stop_balloon_hinting().await
    }

    async fn status(&self) -> Result<HintingStatus, HintingCallError> {
        self.client.balloon_hinting_status().await
    }

    fn hint_freed(&self) -> u64 {
        self.metrics.map_or(0, |metrics| metrics.balloon().hint_freed)
    }

    async fn flush_hint_freed(&self) -> Result<u64, String> {
        match self.metrics {
            Some(metrics) => metrics
                .flush_and_read_balloon(&self.client)
                .await
                .map(|balloon| balloon.hint_freed),
            None => Err("firecracker metrics are not being read".to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HintStop {
    pub timeout: Duration,
    pub grace: Duration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrePauseHint {
    pub timeout: Duration,
    pub stop: HintStop,
    pub firecracker_supported: bool,
}

impl PrePauseHint {
    pub fn from_flag(config: &Value, use_case: &str, firecracker_supported: bool) -> Self {
        let millis = |key: &str, default: Duration| {
            config
                .get(key)
                .and_then(Value::as_f64)
                .map_or(default, |value| Duration::from_millis(value.max(0.0) as u64))
        };
        Self {
            timeout: millis(use_case, Duration::ZERO),
            stop: HintStop {
                timeout: millis("stop", DEFAULT_STOP_TIMEOUT),
                grace: millis("stop_grace", DEFAULT_STOP_GRACE),
            },
            firecracker_supported,
        }
    }
}

#[derive(Debug, Default)]
pub struct HintingState {
    cmd: AtomicI64,
    guest_seen: AtomicI64,
    seen_initialised: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainFailure {
    NotConfigured,
    StartRefused(String),
    InFlight { outcome: &'static str, message: String },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopFailure {
    Unacked(String),
    Failed(String),
}

pub async fn drain(
    api: &impl HintingApi,
    state: &HintingState,
    firecracker_supported: bool,
    deadline: Instant,
) -> Result<(), DrainFailure> {
    if !firecracker_supported {
        return Err(DrainFailure::NotConfigured);
    }
    if let Ok(Ok(caps)) = tokio::time::timeout_at(deadline, api.caps()).await
        && !caps.hinting
    {
        return Err(DrainFailure::NotConfigured);
    }
    state.cmd.store(0, Ordering::SeqCst);
    remember_guest_baseline(api, state, deadline).await;
    start(api, deadline).await?;
    watch_cycle(api, state, deadline).await
}

async fn remember_guest_baseline(api: &impl HintingApi, state: &HintingState, deadline: Instant) {
    if state.seen_initialised.load(Ordering::SeqCst) {
        return;
    }
    if let Ok(Ok(status)) = tokio::time::timeout_at(deadline, api.status()).await {
        state.guest_seen.store(status.guest_cmd, Ordering::SeqCst);
        state.seen_initialised.store(true, Ordering::SeqCst);
    }
}

async fn start(api: &impl HintingApi, deadline: Instant) -> Result<(), DrainFailure> {
    let uncertain = |message: String| DrainFailure::InFlight {
        outcome: "timeout",
        message: format!("balloon hinting cycle possibly in flight: start balloon hinting: {message}"),
    };
    match tokio::time::timeout_at(deadline, api.start()).await {
        Err(_) => Err(uncertain("context deadline exceeded".to_owned())),
        Ok(Err(e)) if e.timed_out => Err(uncertain(e.message)),
        Ok(Err(e)) => Err(DrainFailure::Failed(format!("start balloon hinting: {}", e.message))),
        Ok(Ok(HintingStart::Started)) => Ok(()),
        Ok(Ok(HintingStart::Refused(refused))) => Err(refusal(api, refused).await),
    }
}

async fn refusal(api: &impl HintingApi, refused: String) -> DrainFailure {
    match tokio::time::timeout(CONFIG_READ_TIMEOUT, api.caps()).await {
        Ok(Ok(caps)) if !caps.hinting => DrainFailure::NotConfigured,
        Ok(Ok(_)) => DrainFailure::StartRefused(format!("balloon hinting start refused: {refused}")),
        Ok(Err(e)) => DrainFailure::StartRefused(format!(
            "balloon hinting start refused: balloon config unreadable: {refused}\n{e}"
        )),
        Err(_) => DrainFailure::StartRefused(format!(
            "balloon hinting start refused: balloon config unreadable: {refused}\ncontext deadline exceeded"
        )),
    }
}

async fn watch_cycle(api: &impl HintingApi, state: &HintingState, deadline: Instant) -> Result<(), DrainFailure> {
    let mut cmd = 0;
    let mut engaged = false;
    let polled = poll(api, deadline, |status| {
        state.guest_seen.store(status.guest_cmd, Ordering::SeqCst);
        if cmd == 0 && status.host_cmd >= FIRST_HINT_CMD {
            cmd = status.host_cmd;
            state.cmd.store(cmd, Ordering::SeqCst);
        }
        engaged = engaged || (cmd >= FIRST_HINT_CMD && status.guest_cmd == cmd);
        status.host_cmd == HINT_DONE
    })
    .await;
    match polled {
        Ok(()) => Ok(()),
        Err(Poll::Failed(e)) => Err(DrainFailure::InFlight {
            outcome: "failed",
            message: format!("balloon hinting cycle possibly in flight: balloon hinting status: {e}"),
        }),
        Err(Poll::Deadline) if engaged || cmd == 0 => Err(DrainFailure::InFlight {
            outcome: "timeout",
            message: "balloon hinting cycle possibly in flight: context deadline exceeded".to_owned(),
        }),
        Err(Poll::Deadline) => Err(DrainFailure::InFlight {
            outcome: "timeout-guest-silent",
            message: "balloon hinting cycle possibly in flight: guest did not engage in the hinting cycle: context deadline exceeded".to_owned(),
        }),
    }
}

pub async fn stop(
    api: &impl HintingApi,
    state: &HintingState,
    firecracker_supported: bool,
    grace: Duration,
    deadline: Instant,
) -> Result<(), StopFailure> {
    if !firecracker_supported {
        return Ok(());
    }
    match tokio::time::timeout_at(deadline, api.stop()).await {
        Err(_) => {
            return Err(StopFailure::Failed(
                "stop balloon hinting: context deadline exceeded".to_owned(),
            ));
        }
        Ok(Err(e)) => return Err(StopFailure::Failed(format!("stop balloon hinting: {}", e.message))),
        Ok(Ok(HintingStop::Refused)) => return Ok(()),
        Ok(Ok(HintingStop::Stopped)) => {}
    }
    let cmd = state.cmd.load(Ordering::SeqCst);
    let seen = state.guest_seen.load(Ordering::SeqCst);
    let have_seen = state.seen_initialised.load(Ordering::SeqCst);
    let grace_ends = Instant::now() + grace;
    let mut engaged = false;
    let mut last = 0;
    let polled = poll(api, deadline, |status| {
        last = status.guest_cmd;
        state.guest_seen.store(status.guest_cmd, Ordering::SeqCst);
        engaged = engaged
            || (status.guest_cmd >= FIRST_HINT_CMD
                && (status.guest_cmd == cmd || (cmd == 0 && have_seen && status.guest_cmd != seen)));
        match engaged {
            true => status.guest_cmd == HINT_STOP,
            false => Instant::now() >= grace_ends,
        }
    })
    .await;
    match polled {
        Ok(()) => Ok(()),
        Err(Poll::Deadline) => Err(StopFailure::Unacked(format!(
            "balloon hinting stop not acknowledged by guest: guest_cmd={last} engaged={engaged}: context deadline exceeded"
        ))),
        Err(Poll::Failed(e)) => Err(StopFailure::Failed(format!(
            "stop balloon hinting: acknowledgement poll: balloon hinting status: {e}"
        ))),
    }
}

enum Poll {
    Deadline,
    Failed(String),
}

async fn poll(
    api: &impl HintingApi,
    deadline: Instant,
    mut done: impl FnMut(HintingStatus) -> bool,
) -> Result<(), Poll> {
    let mut backoff = FIRST_BACKOFF;
    loop {
        let status = match tokio::time::timeout_at(deadline, api.status()).await {
            Err(_) => return Err(Poll::Deadline),
            Ok(Err(e)) if e.timed_out => return Err(Poll::Deadline),
            Ok(Err(e)) => return Err(Poll::Failed(e.message)),
            Ok(Ok(status)) => status,
        };
        if done(status) {
            return Ok(());
        }
        if Instant::now() + backoff >= deadline {
            tokio::time::sleep_until(deadline).await;
            return Err(Poll::Deadline);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

pub struct PrePauseDrain<'a, A: HintingApi> {
    pub api: &'a A,
    pub state: &'a HintingState,
    pub hint: PrePauseHint,
    pub sandbox_id: &'a str,
    pub exited: &'a (dyn Fn() -> bool + Sync),
}

impl<A: HintingApi> PrePauseDrain<'_, A> {
    pub async fn run(&self) {
        let started = Instant::now();
        let freed_before = self.api.hint_freed();
        let drained = drain(
            self.api,
            self.state,
            self.hint.firecracker_supported,
            started + self.hint.timeout,
        )
        .await;
        let took = started.elapsed();
        match drained {
            Ok(()) => {
                record_hint_run(PHASE_PRE_PAUSE, "ok", took);
                let freed_after = self.freed_after().await;
                record_hint_freed(PHASE_PRE_PAUSE, freed_after.saturating_sub(freed_before));
            }
            Err(DrainFailure::NotConfigured) => record_hint_run(PHASE_PRE_PAUSE, "not-configured", Duration::ZERO),
            Err(DrainFailure::InFlight { outcome, message }) => {
                self.stop_abandoned().await;
                record_hint_run(PHASE_PRE_PAUSE, outcome, took);
                tracing::error!(sandbox.id = self.sandbox_id, error = %message, "balloon hinting drain did not finish before pause; cycle stopped");
            }
            Err(DrainFailure::StartRefused(message)) => {
                record_hint_run(PHASE_PRE_PAUSE, "refused", took);
                tracing::warn!(sandbox.id = self.sandbox_id, error = %message, "balloon hinting drain refused by FC (continuing pause)");
            }
            Err(DrainFailure::Failed(message)) => {
                record_hint_run(PHASE_PRE_PAUSE, "failed", took);
                tracing::error!(sandbox.id = self.sandbox_id, error = %message, "balloon hinting drain failed (continuing pause)");
            }
        }
    }

    async fn freed_after(&self) -> u64 {
        match tokio::time::timeout(FREED_READ_TIMEOUT, self.api.flush_hint_freed()).await {
            Ok(Ok(freed)) => freed,
            _ => self.api.hint_freed(),
        }
    }

    async fn stop_abandoned(&self) {
        if (self.exited)() {
            record_hint_stop("skipped-exited", Duration::ZERO);
            return;
        }
        if self.hint.stop.timeout.is_zero() {
            record_hint_stop("skipped-disabled", Duration::ZERO);
            tracing::warn!(
                sandbox.id = self.sandbox_id,
                "balloon hinting: cycle left running, stop disabled by flag"
            );
            return;
        }
        let started = Instant::now();
        let stopped = stop(
            self.api,
            self.state,
            self.hint.firecracker_supported,
            self.hint.stop.grace,
            started + self.hint.stop.timeout,
        )
        .await;
        let took = started.elapsed();
        if stopped.is_err() {
            self.wait_for_exit().await;
        }
        match stopped {
            Ok(()) => record_hint_stop("ok", took),
            Err(_) if (self.exited)() => record_hint_stop("skipped-exited", took),
            Err(StopFailure::Unacked(message)) => {
                record_hint_stop("unacked", took);
                tracing::warn!(sandbox.id = self.sandbox_id, error = %message, "balloon hinting: guest did not acknowledge the stop in time");
            }
            Err(StopFailure::Failed(message)) => {
                record_hint_stop("failed", took);
                tracing::error!(sandbox.id = self.sandbox_id, error = %message, "balloon hinting: stopping an abandoned cycle failed");
            }
        }
    }

    async fn wait_for_exit(&self) {
        for _ in 0..EXIT_CHECKS {
            if (self.exited)() {
                return;
            }
            tokio::time::sleep(EXIT_CHECK_INTERVAL).await;
        }
    }
}

impl super::Sandbox {
    pub async fn pre_pause_hint_drain(&self, hint: PrePauseHint) {
        let Some(vm) = self.resources.vm.as_ref().filter(|_| !hint.timeout.is_zero()) else {
            return;
        };
        let api = VmHinting {
            client: vm.client(),
            metrics: vm.metrics(),
        };
        let pid = vm.pid();
        let exited = move || pid.is_none_or(|pid| !std::path::Path::new(&format!("/proc/{pid}")).exists());
        PrePauseDrain {
            api: &api,
            state: &self.hinting,
            hint,
            sandbox_id: &self.config.id,
            exited: &exited,
        }
        .run()
        .await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use parking_lot::Mutex;
    use serde_json::json;

    use super::*;

    struct FakeApi {
        caps: Result<BalloonCaps, String>,
        start: Result<HintingStart, HintingCallError>,
        stop: Result<HintingStop, HintingCallError>,
        statuses: Mutex<VecDeque<HintingStatus>>,
        last: Mutex<HintingStatus>,
    }

    fn api(statuses: &[(i64, i64)]) -> FakeApi {
        FakeApi {
            caps: Ok(BalloonCaps {
                reporting: false,
                hinting: true,
            }),
            start: Ok(HintingStart::Started),
            stop: Ok(HintingStop::Stopped),
            statuses: Mutex::new(
                statuses
                    .iter()
                    .map(|(host_cmd, guest_cmd)| HintingStatus {
                        host_cmd: *host_cmd,
                        guest_cmd: *guest_cmd,
                    })
                    .collect(),
            ),
            last: Mutex::new(HintingStatus::default()),
        }
    }

    impl HintingApi for FakeApi {
        async fn caps(&self) -> Result<BalloonCaps, String> {
            self.caps.clone()
        }

        async fn start(&self) -> Result<HintingStart, HintingCallError> {
            self.start.clone()
        }

        async fn stop(&self) -> Result<HintingStop, HintingCallError> {
            self.stop.clone()
        }

        fn hint_freed(&self) -> u64 {
            0
        }

        async fn flush_hint_freed(&self) -> Result<u64, String> {
            Ok(0)
        }

        async fn status(&self) -> Result<HintingStatus, HintingCallError> {
            let next = self.statuses.lock().pop_front();
            let status = next.unwrap_or(*self.last.lock());
            *self.last.lock() = status;
            Ok(status)
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_millis(200)
    }

    #[tokio::test]
    async fn a_completed_cycle_drains() {
        let api = api(&[(0, 0), (2, 0), (2, 2), (1, 2)]);

        assert_eq!(drain(&api, &HintingState::default(), true, soon()).await, Ok(()));
    }

    #[tokio::test]
    async fn a_balloon_without_hinting_or_an_old_firecracker_is_not_configured() {
        let mut no_hinting = api(&[]);
        no_hinting.caps = Ok(BalloonCaps::default());

        assert_eq!(
            drain(&no_hinting, &HintingState::default(), true, soon()).await,
            Err(DrainFailure::NotConfigured)
        );
        assert_eq!(
            drain(&api(&[]), &HintingState::default(), false, soon()).await,
            Err(DrainFailure::NotConfigured)
        );
    }

    #[tokio::test]
    async fn a_refused_start_reads_the_config_to_tell_why() {
        let mut refused = api(&[]);
        refused.start = Ok(HintingStart::Refused("400".to_owned()));

        let failure = drain(&refused, &HintingState::default(), true, soon()).await;

        assert!(matches!(failure, Err(DrainFailure::StartRefused(_))), "{failure:?}");
    }

    #[tokio::test]
    async fn a_guest_that_never_echoes_the_command_is_silent() {
        let api = api(&[(0, 0), (2, 0)]);

        let failure = drain(&api, &HintingState::default(), true, soon()).await;

        assert!(
            matches!(
                failure,
                Err(DrainFailure::InFlight {
                    outcome: "timeout-guest-silent",
                    ..
                })
            ),
            "{failure:?}"
        );
    }

    #[tokio::test]
    async fn an_engaged_guest_that_runs_out_of_budget_times_out() {
        let api = api(&[(0, 0), (2, 0), (2, 2)]);

        let failure = drain(&api, &HintingState::default(), true, soon()).await;

        assert!(
            matches!(failure, Err(DrainFailure::InFlight { outcome: "timeout", .. })),
            "{failure:?}"
        );
    }

    #[tokio::test]
    async fn a_stop_waits_for_an_engaged_guest_to_echo_the_stop() {
        let state = HintingState::default();
        state.cmd.store(2, Ordering::SeqCst);
        let acked = api(&[(1, 2), (1, 0)]);
        let silent = api(&[(1, 2)]);

        assert_eq!(stop(&acked, &state, true, Duration::ZERO, soon()).await, Ok(()));
        assert!(matches!(
            stop(&silent, &state, true, Duration::ZERO, soon()).await,
            Err(StopFailure::Unacked(_))
        ));
    }

    #[tokio::test]
    async fn a_stop_on_a_guest_that_never_engaged_waits_only_the_grace() {
        let state = HintingState::default();
        state.cmd.store(2, Ordering::SeqCst);
        let api = api(&[(1, 0)]);

        assert_eq!(
            stop(&api, &state, true, Duration::from_millis(20), soon()).await,
            Ok(())
        );
    }

    #[test]
    fn the_flag_gives_the_budget_per_use_case_and_the_shared_stop() {
        let config = json!({"enabled": true, "pause": 500, "build": "x", "stop": 3000, "stop_grace": -5});

        assert_eq!(
            PrePauseHint::from_flag(&config, USE_CASE_PAUSE, true),
            PrePauseHint {
                timeout: Duration::from_millis(500),
                stop: HintStop {
                    timeout: Duration::from_secs(3),
                    grace: Duration::ZERO
                },
                firecracker_supported: true
            }
        );
        assert_eq!(
            PrePauseHint::from_flag(&config, USE_CASE_BUILD, true).timeout,
            Duration::ZERO
        );
        assert_eq!(
            PrePauseHint::from_flag(&Value::Null, USE_CASE_PAUSE, false).stop,
            HintStop {
                timeout: DEFAULT_STOP_TIMEOUT,
                grace: DEFAULT_STOP_GRACE
            }
        );
    }
}
