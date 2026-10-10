use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{Cycle, DrainFailure, HintRecord, HintRunner, HintStop, SandboxHinting};
use crate::firecracker::client::BalloonCaps;
use crate::memory::server::ServeStats;
use crate::sandbox::seal::SealSlot;

const PHASE_PERIODIC: &str = "periodic";
pub const DISABLED_POLL: Duration = Duration::from_secs(30);
pub const GATE_SLOW_POLL: Duration = Duration::from_secs(300);
const MAX_GATE_RETRIES: u32 = 10;
const GATE_READ_TIMEOUT: Duration = Duration::from_millis(500);
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);
const WINDOW_RUN: &str = "run";
const WINDOW_INTERVAL: &str = "interval";
const LABEL_IDLE: &str = "idle";
const LABEL_BACKOFF: &str = "skipped-backoff";
const LABEL_OBSERVE_ONLY: &str = "observe-only";
const LABEL_HOST_BUSY: &str = "skipped-host-busy";
const BALLOON_UNKNOWN: &str = "skipped-balloon-unknown";

static COMPLAINTS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

pub static HOST_ADMISSION: LazyLock<Arc<HostAdmission>> = LazyLock::new(Arc::default);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeriodicConfig {
    pub interval: Duration,
    pub timeout: Duration,
    pub quiet_after_start: Duration,
    pub silent_runs: u32,
    pub unresponsive_retry: Duration,
    pub host_slots: usize,
    pub stop: HintStop,
    pub observe_only: bool,
}

struct PeriodicBlock<'a> {
    value: &'a Value,
}

impl PeriodicBlock<'_> {
    fn field(&self, key: &str) -> Option<&Value> {
        self.value.get(key).filter(|field| !field.is_null())
    }

    fn number(&self, key: &str, default: i64) -> i64 {
        match self.field(key) {
            Some(field) if field.is_number() => field.as_f64().map_or(default, |number| number as i64),
            Some(field) => {
                complain(key, "not a number; using the default", field);
                default
            }
            None => default,
        }
    }

    fn millis(&self, key: &str, default: i64) -> Duration {
        Duration::from_millis(self.number(key, default).max(0) as u64)
    }

    fn boolean(&self, key: &str) -> bool {
        match self.field(key) {
            Some(Value::Bool(flag)) => *flag,
            Some(field) => {
                complain(key, "not a boolean; using the default", field);
                false
            }
            None => false,
        }
    }
}

fn complain(field: &str, why: &str, value: &Value) {
    if COMPLAINTS.lock().insert(format!("{field}:{why}")) {
        tracing::warn!(field, value = %value, "free-page-hinting-config periodic: {why}");
    }
}

impl PeriodicConfig {
    pub fn from_flag(config: &Value) -> Self {
        let value = config.get("periodic").unwrap_or(&Value::Null);
        if !value.is_null() && !value.is_object() {
            complain("periodic", "not an object; periodic hinting off", value);
        }
        let block = PeriodicBlock { value };
        let timeout = block.number("timeout", DEFAULT_TIMEOUT.as_millis() as i64);
        Self {
            interval: block.millis("interval", 0),
            timeout: match timeout > 0 {
                true => Duration::from_millis(timeout as u64),
                false => DEFAULT_TIMEOUT,
            },
            quiet_after_start: block.millis("quiet_after_start", 10_000),
            silent_runs: block.number("silent_runs", 3).clamp(0, i64::from(u32::MAX)) as u32,
            unresponsive_retry: block.millis("unresponsive_retry", 300_000),
            host_slots: block.number("host_slots", 16).max(0) as usize,
            stop: HintStop::from_flag(config),
            observe_only: block.boolean("observe_only"),
        }
        .with_interval_floor(&block)
    }

    fn with_interval_floor(self, block: &PeriodicBlock) -> Self {
        if self.interval.is_zero() || self.interval >= MIN_INTERVAL {
            return self;
        }
        complain(
            "interval",
            "below the one-second floor; periodic hinting off",
            block.field("interval").unwrap_or(&Value::Null),
        );
        Self {
            interval: Duration::ZERO,
            ..self
        }
    }

    pub fn enabled(&self) -> bool {
        !self.interval.is_zero()
    }
}

#[derive(Debug, Default)]
pub struct HostAdmission {
    in_flight: Mutex<usize>,
}

pub struct AdmissionSlot {
    admission: Arc<HostAdmission>,
}

impl Drop for AdmissionSlot {
    fn drop(&mut self) {
        *self.admission.in_flight.lock() -= 1;
    }
}

impl HostAdmission {
    pub fn try_acquire(self: &Arc<Self>, capacity: usize) -> Option<AdmissionSlot> {
        let mut in_flight = self.in_flight.lock();
        if capacity > 0 && *in_flight >= capacity {
            return None;
        }
        *in_flight += 1;
        Some(AdmissionSlot {
            admission: self.clone(),
        })
    }
}

#[derive(Debug, Default)]
pub struct CheckpointMarks {
    in_flight: AtomicBool,
    ended_at: Mutex<Option<Instant>>,
}

impl CheckpointMarks {
    pub fn begin(&self) -> bool {
        self.in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end(&self) {
        *self.ended_at.lock() = Some(Instant::now());
        self.in_flight.store(false, Ordering::SeqCst);
    }

    fn active(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }

    fn last_ended(&self) -> Option<Instant> {
        *self.ended_at.lock()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TickInput {
    pub now: Instant,
    pub started_at: Option<Instant>,
    pub last_checkpoint_at: Option<Instant>,
    pub checkpoint_active: bool,
    pub memory_seal_pending: bool,
    pub rootfs_seal_pending: bool,
}

pub fn skip_reason(config: &PeriodicConfig, input: &TickInput) -> Option<&'static str> {
    let quiet = |since: Option<Instant>| {
        since.is_some_and(|at| input.now.saturating_duration_since(at) < config.quiet_after_start)
    };
    [
        (input.checkpoint_active, "skipped-checkpoint-in-flight"),
        (input.memory_seal_pending, "skipped-memory-seal-pending"),
        (input.rootfs_seal_pending, "skipped-rootfs-seal-pending"),
        (quiet(input.started_at), "skipped-quiet-after-start"),
        (quiet(input.last_checkpoint_at), "skipped-quiet-after-checkpoint"),
    ]
    .into_iter()
    .find_map(|(skip, reason)| skip.then_some(reason))
}

pub fn balloon_gate(caps: &Result<BalloonCaps, String>) -> Option<&'static str> {
    match caps {
        Err(_) => Some(BALLOON_UNKNOWN),
        Ok(caps) if !caps.hinting => Some("skipped-not-configured"),
        Ok(caps) if caps.reporting => Some("skipped-reporting-active"),
        Ok(_) => None,
    }
}

pub type ServeSource = Arc<dyn Fn() -> ServeStats + Send + Sync>;

pub struct SandboxSignals {
    pub started_at: Instant,
    pub checkpoints: Arc<CheckpointMarks>,
    pub memory_seal: SealSlot,
    pub rootfs_seal: SealSlot,
    pub serve: ServeSource,
}

impl SandboxSignals {
    fn input(&self, now: Instant) -> TickInput {
        let checkpoint_active = self.checkpoints.active();
        TickInput {
            now,
            started_at: Some(self.started_at),
            last_checkpoint_at: self.checkpoints.last_ended(),
            checkpoint_active,
            memory_seal_pending: self.memory_seal.pending(),
            rootfs_seal_pending: self.rootfs_seal.pending(),
        }
    }

    fn serve_stats(&self) -> ServeStats {
        (self.serve)()
    }
}

pub struct HintGates {
    pub firecracker_supported: bool,
    pub poll: Duration,
    pub slow: Duration,
    pub admission: Arc<HostAdmission>,
}

impl HintGates {
    pub fn production(firecracker_supported: bool) -> Self {
        Self {
            firecracker_supported,
            poll: DISABLED_POLL,
            slow: GATE_SLOW_POLL,
            admission: HOST_ADMISSION.clone(),
        }
    }
}

pub type ConfigSource = Box<dyn Fn() -> PeriodicConfig + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintResult {
    Ok,
    Aborted,
    Failed,
    GuestSilent,
    Exit,
}

#[derive(Debug, Clone, Copy)]
struct Attempt {
    result: HintResult,
    outcome: Option<&'static str>,
    after: ServeStats,
    label: Option<&'static str>,
}

impl Attempt {
    fn nothing(result: HintResult) -> Self {
        Self {
            result,
            outcome: None,
            after: ServeStats::default(),
            label: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicksEnd {
    Stop,
    Disabled,
}

struct Ticking {
    pending: Option<Attempt>,
    prev_tick: Option<ServeStats>,
    prev_label: &'static str,
    silent: u32,
    retry_at: Option<Instant>,
}

pub struct PeriodicHinting<R: HintRunner> {
    pub runner: R,
    pub hinting: Arc<SandboxHinting>,
    pub signals: SandboxSignals,
    pub gates: HintGates,
    pub config: ConfigSource,
    pub record: Arc<dyn HintRecord>,
    pub sandbox_id: String,
    pub cancel: CancellationToken,
    pub unresponsive: AtomicBool,
    pub warned: AtomicBool,
}

impl<R: HintRunner> PeriodicHinting<R> {
    pub async fn run(&self) {
        self.run_gated().await;
        self.drop_latch();
    }

    async fn run_gated(&self) {
        if !self.gates.firecracker_supported {
            if (self.config)().enabled() {
                self.record_run("skipped-fc-unsupported", Duration::ZERO);
            }
            return;
        }
        let Some(mut config) = self.wait_until_enabled(false).await else {
            return;
        };
        if !self.pass_balloon_gate().await {
            return;
        }
        loop {
            tracing::info!(
                sandbox.id = %self.sandbox_id,
                interval = ?config.interval,
                timeout = ?config.timeout,
                quiet_after_start = ?config.quiet_after_start,
                "periodic free-page hinting on"
            );
            if self.ticks(config).await == TicksEnd::Stop {
                return;
            }
            self.drop_latch();
            match self.wait_until_enabled(true).await {
                Some(enabled) => config = enabled,
                None => return,
            }
        }
    }

    async fn wait_until_enabled(&self, record: bool) -> Option<PeriodicConfig> {
        let config = (self.config)();
        if config.enabled() {
            return Some(config);
        }
        if self.cancel.is_cancelled() {
            return None;
        }
        if record {
            self.record_run("skipped-disabled", Duration::ZERO);
        }
        while self.sleep(self.gates.poll).await {
            let config = (self.config)();
            if config.enabled() {
                return Some(config);
            }
        }
        None
    }

    async fn pass_balloon_gate(&self) -> bool {
        for attempt in 1.. {
            let caps = self.read_caps().await;
            if self.cancel.is_cancelled() {
                return false;
            }
            let Some(outcome) = balloon_gate(&caps) else {
                return true;
            };
            self.record_run(outcome, Duration::ZERO);
            if outcome != BALLOON_UNKNOWN {
                return false;
            }
            let wait = match attempt >= MAX_GATE_RETRIES {
                true => self.gates.slow.max(self.gates.poll),
                false => self.gates.poll,
            };
            if !self.sleep(wait).await {
                return false;
            }
        }
        false
    }

    async fn read_caps(&self) -> Result<BalloonCaps, String> {
        tokio::select! {
            () = self.cancel.cancelled() => Err("context canceled".to_owned()),
            read = tokio::time::timeout(GATE_READ_TIMEOUT, self.runner.caps()) => {
                read.unwrap_or_else(|_| Err("context deadline exceeded".to_owned()))
            }
        }
    }

    async fn ticks(&self, mut config: PeriodicConfig) -> TicksEnd {
        let mut next = Instant::now() + config.interval + jitter(config.interval);
        let mut ticking = Ticking {
            pending: None,
            prev_tick: None,
            prev_label: "",
            silent: 0,
            retry_at: None,
        };
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => return TicksEnd::Stop,
                () = tokio::time::sleep_until(next) => {}
            }
            next = Instant::now() + config.interval;
            config = (self.config)();
            if !config.enabled() {
                return TicksEnd::Disabled;
            }
            if self.tick(&config, &mut ticking).await {
                return TicksEnd::Stop;
            }
        }
    }

    async fn tick(&self, config: &PeriodicConfig, ticking: &mut Ticking) -> bool {
        let now = Instant::now();
        let input = self.signals.input(now);
        let current = self.signals.serve_stats();
        self.close_interval(ticking, current);
        if self.unresponsive.load(Ordering::SeqCst) && ticking.retry_at.is_some_and(|at| now < at) {
            ticking.prev_label = LABEL_BACKOFF;
            return false;
        }
        if let Some(reason) = skip_reason(config, &input) {
            self.record_run(reason, Duration::ZERO);
            ticking.prev_label = reason;
            return false;
        }
        if config.observe_only {
            self.record_run("skipped-observe-only", Duration::ZERO);
            ticking.prev_label = LABEL_OBSERVE_ONLY;
            return false;
        }
        let attempt = self.hint_once(config).await;
        match (attempt.outcome, attempt.label) {
            (Some(_), _) => {
                ticking.pending = Some(attempt);
                ticking.prev_tick = None;
            }
            (None, Some(label)) => ticking.prev_label = label,
            (None, None) => {}
        }
        let stop = self.judge(config, ticking, attempt.result);
        if attempt.result != HintResult::Aborted && self.unresponsive.load(Ordering::SeqCst) {
            ticking.retry_at = Some(Instant::now() + config.unresponsive_retry.max(config.interval));
        }
        stop
    }

    fn close_interval(&self, ticking: &mut Ticking, current: ServeStats) {
        match (ticking.pending.take(), ticking.prev_tick) {
            (Some(attempt), _) => self.record_faults(
                WINDOW_INTERVAL,
                attempt.outcome.unwrap_or_default(),
                &attempt.after,
                &current,
            ),
            (None, Some(prev)) => self.record_faults(WINDOW_INTERVAL, ticking.prev_label, &prev, &current),
            (None, None) => {}
        }
        ticking.prev_tick = Some(current);
        ticking.prev_label = LABEL_IDLE;
    }

    fn judge(&self, config: &PeriodicConfig, ticking: &mut Ticking, result: HintResult) -> bool {
        match result {
            HintResult::Exit => true,
            HintResult::Aborted => self.runner.exited(),
            HintResult::Ok => {
                ticking.silent = 0;
                self.warned.store(false, Ordering::SeqCst);
                if self.unresponsive.swap(false, Ordering::SeqCst) {
                    self.record.latch("released");
                    tracing::info!(sandbox.id = %self.sandbox_id, "periodic free-page hinting: guest engaged again");
                }
                false
            }
            HintResult::GuestSilent => {
                ticking.silent += 1;
                if config.silent_runs > 0
                    && ticking.silent >= config.silent_runs
                    && !self.unresponsive.swap(true, Ordering::SeqCst)
                {
                    self.record.latch("latched");
                    tracing::warn!(
                        sandbox.id = %self.sandbox_id,
                        runs = ticking.silent,
                        retry = ?config.unresponsive_retry,
                        "periodic free-page hinting: guest never engages; backing off"
                    );
                }
                false
            }
            HintResult::Failed => false,
        }
    }

    async fn hint_once(&self, config: &PeriodicConfig) -> Attempt {
        let _run = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Attempt::nothing(HintResult::Exit),
            run = self.hinting.runs.lock() => run,
        };
        if self.cancel.is_cancelled() {
            return Attempt::nothing(HintResult::Exit);
        }
        let Some(_slot) = self.gates.admission.try_acquire(config.host_slots) else {
            self.record_run(LABEL_HOST_BUSY, Duration::ZERO);
            return Attempt {
                label: Some(LABEL_HOST_BUSY),
                ..Attempt::nothing(HintResult::Aborted)
            };
        };
        let before = self.signals.serve_stats();
        let started = Instant::now();
        let freed_before = self.runner.hint_freed();
        let drained = tokio::select! {
            drained = self.runner.drain(started + config.timeout) => drained.map_err(Some),
            () = self.cancel.cancelled() => Err(None),
        };
        let took = started.elapsed();
        let run = RunWindow { before, took };
        match drained {
            Ok(()) => {
                let attempt = self.attempt(&run, "ok", HintResult::Ok);
                self.cycle().record_freed(PHASE_PERIODIC, freed_before).await;
                attempt
            }
            Err(Some(DrainFailure::NotConfigured)) => {
                self.record_run("skipped-not-configured", Duration::ZERO);
                Attempt::nothing(HintResult::Exit)
            }
            Err(None) => {
                self.cycle().stop_abandoned(config.stop).await;
                Attempt::nothing(HintResult::Aborted)
            }
            Err(Some(DrainFailure::InFlight { outcome, message })) => {
                self.in_flight(config, &run, outcome, message).await
            }
            Err(Some(_)) if self.going_away() => Attempt::nothing(HintResult::Aborted),
            Err(Some(DrainFailure::StartRefused {
                message,
                config_unknown: false,
            })) => {
                let attempt = self.attempt(&run, "refused", HintResult::GuestSilent);
                self.hint_log("periodic free-page hinting start refused", &message);
                attempt
            }
            Err(Some(DrainFailure::StartRefused { message, .. } | DrainFailure::Failed(message))) => {
                let attempt = self.attempt(&run, "failed", HintResult::Failed);
                self.hint_log("periodic free-page hinting run failed", &message);
                attempt
            }
        }
    }

    async fn in_flight(
        &self,
        config: &PeriodicConfig,
        run: &RunWindow,
        outcome: &'static str,
        message: String,
    ) -> Attempt {
        self.cycle().stop_abandoned(config.stop).await;
        if self.going_away() {
            return Attempt::nothing(HintResult::Aborted);
        }
        let (result, log) = match outcome {
            "timeout-guest-silent" => (
                HintResult::GuestSilent,
                "periodic free-page hinting run timed out without the guest engaging; cycle stopped",
            ),
            "timeout" => (
                HintResult::Failed,
                "periodic free-page hinting run timed out; cycle stopped",
            ),
            _ => (
                HintResult::Failed,
                "periodic free-page hinting run failed after start; cycle stopped",
            ),
        };
        let attempt = self.attempt(run, outcome, result);
        self.hint_log(log, &message);
        attempt
    }

    fn attempt(&self, run: &RunWindow, outcome: &'static str, result: HintResult) -> Attempt {
        let after = self.signals.serve_stats();
        self.record_run(outcome, run.took);
        self.record_faults(WINDOW_RUN, outcome, &run.before, &after);
        Attempt {
            result,
            outcome: Some(outcome),
            after,
            label: None,
        }
    }

    fn going_away(&self) -> bool {
        self.hinting.stopping.load(Ordering::SeqCst) || self.runner.exited()
    }

    fn cycle(&self) -> Cycle<'_, R> {
        Cycle {
            runner: &self.runner,
            record: self.record.as_ref(),
            stopping: &self.hinting.stopping,
            sandbox_id: &self.sandbox_id,
        }
    }

    fn hint_log(&self, message: &'static str, error: &str) {
        let first = !self.unresponsive.load(Ordering::SeqCst) && !self.warned.swap(true, Ordering::SeqCst);
        match first {
            true => tracing::warn!(sandbox.id = %self.sandbox_id, error, "{message}"),
            false => tracing::debug!(sandbox.id = %self.sandbox_id, error, "{message}"),
        }
    }

    fn drop_latch(&self) {
        self.warned.store(false, Ordering::SeqCst);
        if self.unresponsive.swap(false, Ordering::SeqCst) {
            self.record.latch("dropped");
        }
    }

    fn record_run(&self, outcome: &'static str, took: Duration) {
        self.record.run(PHASE_PERIODIC, outcome, took);
    }

    fn record_faults(&self, window: &'static str, outcome: &'static str, before: &ServeStats, after: &ServeStats) {
        self.record
            .faults(window, outcome, super::FaultDelta::between(before, after));
    }

    async fn sleep(&self, duration: Duration) -> bool {
        tokio::select! {
            () = self.cancel.cancelled() => false,
            () = tokio::time::sleep(duration.max(Duration::from_millis(1))) => true,
        }
    }
}

struct RunWindow {
    before: ServeStats,
    took: Duration,
}

fn jitter(interval: Duration) -> Duration {
    let span = (interval / 2).as_nanos().max(1) as u64;
    Duration::from_nanos(rand::random::<u64>() % span)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};

    use futures::FutureExt;
    use futures::future::BoxFuture;
    use serde_json::json;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::sandbox::hinting::{FaultDelta, StopFailure};

    type DrainScript = Box<dyn Fn(Instant) -> BoxFuture<'static, Result<(), DrainFailure>> + Send + Sync>;

    #[derive(Default)]
    struct FakeRunner {
        caps: Mutex<VecDeque<Result<BalloonCaps, String>>>,
        drain: Option<DrainScript>,
        drains: AtomicU32,
        stops: AtomicU32,
        exited: AtomicBool,
        freed: AtomicU64,
    }

    impl HintRunner for Arc<FakeRunner> {
        async fn caps(&self) -> Result<BalloonCaps, String> {
            let mut caps = self.caps.lock();
            match caps.len() {
                0 => Ok(BalloonCaps {
                    reporting: false,
                    hinting: true,
                }),
                1 => caps[0].clone(),
                _ => caps.pop_front().unwrap_or_else(|| Err("empty".to_owned())),
            }
        }

        async fn drain(&self, deadline: Instant) -> Result<(), DrainFailure> {
            self.drains.fetch_add(1, Ordering::SeqCst);
            match &self.drain {
                Some(script) => script(deadline).await,
                None => {
                    self.freed.fetch_add(1 << 20, Ordering::SeqCst);
                    Ok(())
                }
            }
        }

        async fn stop(&self, _grace: Duration, _deadline: Instant) -> Result<(), StopFailure> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn exited(&self) -> bool {
            self.exited.load(Ordering::SeqCst)
        }

        fn hint_freed(&self) -> u64 {
            self.freed.load(Ordering::SeqCst)
        }

        async fn flush_hint_freed(&self) -> Result<u64, String> {
            Ok(self.hint_freed())
        }
    }

    #[derive(Default)]
    struct Observed {
        runs: Mutex<Vec<&'static str>>,
        stops: Mutex<Vec<&'static str>>,
        latches: Mutex<Vec<&'static str>>,
        freed: Mutex<Vec<u64>>,
        faults: Mutex<Vec<(&'static str, &'static str, FaultDelta)>>,
    }

    impl HintRecord for Observed {
        fn run(&self, phase: &'static str, outcome: &'static str, _took: Duration) {
            assert_eq!(phase, PHASE_PERIODIC);
            self.runs.lock().push(outcome);
        }

        fn stop(&self, outcome: &'static str, _took: Duration) {
            self.stops.lock().push(outcome);
        }

        fn freed(&self, _phase: &'static str, bytes: u64) {
            self.freed.lock().push(bytes);
        }

        fn latch(&self, state: &'static str) {
            self.latches.lock().push(state);
        }

        fn faults(&self, window: &'static str, outcome: &'static str, faults: FaultDelta) {
            self.faults.lock().push((window, outcome, faults));
        }
    }

    impl Observed {
        fn runs(&self) -> Vec<&'static str> {
            self.runs.lock().clone()
        }

        fn count(&self, outcome: &str) -> usize {
            self.runs().iter().filter(|run| **run == outcome).count()
        }

        fn served(&self, window: &str, outcome: &str) -> Vec<i64> {
            self.faults
                .lock()
                .iter()
                .filter(|(w, o, _)| *w == window && *o == outcome)
                .map(|(_, _, faults)| faults.served)
                .collect()
        }
    }

    struct Harness {
        runner: Arc<FakeRunner>,
        observed: Arc<Observed>,
        hinting: Arc<SandboxHinting>,
        checkpoints: Arc<CheckpointMarks>,
        served: Arc<AtomicI64>,
        admission: Arc<HostAdmission>,
        cancel: CancellationToken,
    }

    fn test_config() -> PeriodicConfig {
        PeriodicConfig {
            interval: Duration::from_millis(20),
            timeout: Duration::from_millis(50),
            quiet_after_start: Duration::from_secs(10),
            silent_runs: 3,
            unresponsive_retry: Duration::from_millis(30),
            host_slots: 0,
            stop: HintStop {
                timeout: Duration::from_secs(2),
                grace: Duration::from_millis(100),
            },
            observe_only: false,
        }
    }

    fn fixed(config: PeriodicConfig) -> ConfigSource {
        Box::new(move || config)
    }

    fn toggled(off: Arc<AtomicBool>) -> ConfigSource {
        Box::new(move || PeriodicConfig {
            interval: match off.load(Ordering::SeqCst) {
                true => Duration::ZERO,
                false => test_config().interval,
            },
            ..test_config()
        })
    }

    fn silent() -> DrainFailure {
        DrainFailure::InFlight {
            outcome: "timeout-guest-silent",
            message: "guest did not engage".to_owned(),
        }
    }

    fn scripted(script: impl Fn() -> Result<(), DrainFailure> + Send + Sync + 'static) -> FakeRunner {
        FakeRunner {
            drain: Some(Box::new(move |_| futures::future::ready(script()).boxed())),
            ..FakeRunner::default()
        }
    }

    fn hanging() -> FakeRunner {
        FakeRunner {
            drain: Some(Box::new(|_| std::future::pending().boxed())),
            ..FakeRunner::default()
        }
    }

    fn harness(runner: FakeRunner) -> Harness {
        Harness {
            runner: Arc::new(runner),
            observed: Arc::default(),
            hinting: Arc::default(),
            checkpoints: Arc::default(),
            served: Arc::default(),
            admission: Arc::default(),
            cancel: CancellationToken::new(),
        }
    }

    impl Harness {
        fn looping(&self, config: ConfigSource) -> PeriodicHinting<Arc<FakeRunner>> {
            let served = self.served.clone();
            PeriodicHinting {
                runner: self.runner.clone(),
                hinting: self.hinting.clone(),
                signals: SandboxSignals {
                    started_at: Instant::now() - Duration::from_secs(60),
                    checkpoints: self.checkpoints.clone(),
                    memory_seal: SealSlot::default(),
                    rootfs_seal: SealSlot::default(),
                    serve: Arc::new(move || ServeStats {
                        pages: served.load(Ordering::SeqCst),
                        ..ServeStats::default()
                    }),
                },
                gates: HintGates {
                    firecracker_supported: true,
                    poll: Duration::from_millis(10),
                    slow: Duration::from_millis(30),
                    admission: self.admission.clone(),
                },
                config,
                record: self.observed.clone(),
                sandbox_id: "fph-test".to_owned(),
                cancel: self.cancel.clone(),
                unresponsive: AtomicBool::new(false),
                warned: AtomicBool::new(false),
            }
        }

        fn spawn(&self, hinting: PeriodicHinting<Arc<FakeRunner>>) -> JoinHandle<()> {
            tokio::spawn(async move { hinting.run().await })
        }

        async fn run_for(&self, config: ConfigSource, duration: Duration) {
            let task = self.spawn(self.looping(config));
            tokio::time::sleep(duration).await;
            self.cancel.cancel();
            task.await.unwrap();
        }

        fn drains(&self) -> u32 {
            self.runner.drains.load(Ordering::SeqCst)
        }

        fn stops(&self) -> u32 {
            self.runner.stops.load(Ordering::SeqCst)
        }
    }

    fn input(now: Instant) -> TickInput {
        TickInput {
            now,
            started_at: None,
            last_checkpoint_at: None,
            checkpoint_active: false,
            memory_seal_pending: false,
            rootfs_seal_pending: false,
        }
    }

    #[test]
    fn the_skip_reason_follows_go_order() {
        let now = Instant::now() + Duration::from_secs(3600);
        let old = Some(now - Duration::from_secs(60));
        let config = test_config();
        let cases = [
            (
                TickInput {
                    started_at: old,
                    last_checkpoint_at: old,
                    ..input(now)
                },
                None,
            ),
            (input(now), None),
            (
                TickInput {
                    started_at: old,
                    checkpoint_active: true,
                    memory_seal_pending: true,
                    ..input(now)
                },
                Some("skipped-checkpoint-in-flight"),
            ),
            (
                TickInput {
                    started_at: old,
                    memory_seal_pending: true,
                    ..input(now)
                },
                Some("skipped-memory-seal-pending"),
            ),
            (
                TickInput {
                    started_at: old,
                    rootfs_seal_pending: true,
                    ..input(now)
                },
                Some("skipped-rootfs-seal-pending"),
            ),
            (
                TickInput {
                    started_at: Some(now - Duration::from_secs(1)),
                    ..input(now)
                },
                Some("skipped-quiet-after-start"),
            ),
            (
                TickInput {
                    started_at: old,
                    last_checkpoint_at: Some(now - Duration::from_secs(2)),
                    ..input(now)
                },
                Some("skipped-quiet-after-checkpoint"),
            ),
        ];

        for (tick, reason) in cases {
            assert_eq!(skip_reason(&config, &tick), reason, "{tick:?}");
        }
    }

    #[test]
    fn the_balloon_gate_fails_closed() {
        let caps = |reporting, hinting| Ok(BalloonCaps { reporting, hinting });

        assert_eq!(balloon_gate(&caps(false, true)), None);
        assert_eq!(balloon_gate(&caps(true, true)), Some("skipped-reporting-active"));
        assert_eq!(balloon_gate(&caps(false, false)), Some("skipped-not-configured"));
        assert_eq!(balloon_gate(&caps(true, false)), Some("skipped-not-configured"));
        assert_eq!(
            balloon_gate(&Err("api busy".to_owned())),
            Some("skipped-balloon-unknown")
        );
    }

    #[test]
    fn the_flag_block_takes_go_defaults_and_floors() {
        let config = json!({"stop": 3000, "stop_grace": 50, "periodic": {"interval": 5000, "observe_only": true}});

        assert_eq!(
            PeriodicConfig::from_flag(&config),
            PeriodicConfig {
                interval: Duration::from_secs(5),
                timeout: Duration::from_millis(500),
                quiet_after_start: Duration::from_secs(10),
                silent_runs: 3,
                unresponsive_retry: Duration::from_secs(300),
                host_slots: 16,
                stop: HintStop {
                    timeout: Duration::from_secs(3),
                    grace: Duration::from_millis(50),
                },
                observe_only: true,
            }
        );
        let floored = json!({"periodic": {"interval": 500, "timeout": -1, "silent_runs": -2, "host_slots": "x"}});
        let floored = PeriodicConfig::from_flag(&floored);
        assert!(!floored.enabled(), "below the one-second floor");
        assert_eq!(floored.timeout, Duration::from_millis(500));
        assert_eq!(floored.silent_runs, 0);
        assert_eq!(floored.host_slots, 16);
        assert!(!PeriodicConfig::from_flag(&json!({"periodic": 5})).enabled());
        assert!(!PeriodicConfig::from_flag(&Value::Null).enabled());
    }

    #[test]
    fn host_admission_bounds_the_runs_in_flight() {
        let admission = Arc::new(HostAdmission::default());

        let first = admission.try_acquire(1);
        assert!(first.is_some());
        assert!(admission.try_acquire(1).is_none());
        assert!(admission.try_acquire(0).is_some(), "zero capacity is unbounded");
        drop(first);
        assert!(admission.try_acquire(1).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn completed_runs_are_recorded_with_what_they_freed() {
        let h = harness(FakeRunner::default());

        h.run_for(fixed(test_config()), Duration::from_millis(200)).await;

        assert!(h.drains() >= 3, "{}", h.drains());
        assert!(h.observed.runs().iter().all(|run| *run == "ok"));
        assert_eq!(h.stops(), 0, "completed cycles are never stopped");
        assert_eq!(*h.observed.freed.lock(), vec![1 << 20; h.observed.count("ok")]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_run_frees_nothing_on_the_record() {
        let h = harness(scripted(|| Err(silent())));

        let attempt = h.looping(fixed(test_config())).hint_once(&test_config()).await;

        assert_eq!(attempt.result, HintResult::GuestSilent);
        assert!(h.observed.freed.lock().is_empty());
        assert_eq!(h.stops(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn run_faults_land_in_the_run_window_and_the_interval_after_it() {
        let served = Arc::new(AtomicI64::new(0));
        let pages = served.clone();
        let mut h = harness(scripted(move || {
            pages.fetch_add(3, Ordering::SeqCst);
            Ok(())
        }));
        h.served = served;

        h.run_for(fixed(test_config()), Duration::from_millis(100)).await;

        assert!(h.observed.served(WINDOW_RUN, "ok").iter().all(|n| *n == 3));
        assert!(!h.observed.served(WINDOW_RUN, "ok").is_empty());
        assert!(h.observed.served(WINDOW_INTERVAL, "ok").iter().all(|n| *n == 0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_busy_tick_labels_the_interval_after_it() {
        let h = harness(FakeRunner::default());
        let _taken = h.admission.try_acquire(1);
        let task = h.spawn(h.looping(fixed(PeriodicConfig {
            host_slots: 1,
            ..test_config()
        })));

        tokio::time::sleep(Duration::from_millis(80)).await;
        h.served.fetch_add(6, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        h.cancel.cancel();
        task.await.unwrap();

        assert!(h.observed.count(LABEL_HOST_BUSY) >= 2);
        assert!(h.observed.served(WINDOW_INTERVAL, LABEL_HOST_BUSY).contains(&6));
        assert!(h.observed.served(WINDOW_INTERVAL, LABEL_IDLE).is_empty());
        assert_eq!(h.drains(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_stopped_at_its_budget_records_its_run_window() {
        let served = Arc::new(AtomicI64::new(0));
        let pages = served.clone();
        let mut h = harness(FakeRunner {
            drain: Some(Box::new(move |deadline| {
                pages.fetch_add(7, Ordering::SeqCst);
                async move {
                    tokio::time::sleep_until(deadline).await;
                    Err(DrainFailure::InFlight {
                        outcome: "timeout",
                        message: "context deadline exceeded".to_owned(),
                    })
                }
                .boxed()
            })),
            ..FakeRunner::default()
        });
        h.served = served;

        let attempt = h.looping(fixed(test_config())).hint_once(&test_config()).await;

        assert_eq!(attempt.result, HintResult::Failed);
        assert_eq!(h.observed.served(WINDOW_RUN, "timeout"), vec![7]);
        assert!(h.observed.served(WINDOW_RUN, "ok").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn observe_only_ticks_like_a_treated_sandbox_but_never_hints() {
        let h = harness(FakeRunner::default());
        let task = h.spawn(h.looping(fixed(PeriodicConfig {
            observe_only: true,
            ..test_config()
        })));

        tokio::time::sleep(Duration::from_millis(80)).await;
        h.served.fetch_add(9, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(h.checkpoints.begin());
        tokio::time::sleep(Duration::from_millis(40)).await;
        h.served.fetch_add(4, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;
        h.cancel.cancel();
        task.await.unwrap();

        assert!(h.observed.count("skipped-observe-only") >= 2);
        assert!(h.observed.served(WINDOW_INTERVAL, LABEL_OBSERVE_ONLY).contains(&9));
        assert!(!h.observed.served(WINDOW_INTERVAL, LABEL_OBSERVE_ONLY).contains(&4));
        assert!(
            h.observed
                .served(WINDOW_INTERVAL, "skipped-checkpoint-in-flight")
                .contains(&4)
        );
        assert_eq!(h.drains(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_sandbox_inside_its_quiet_window_is_never_drained() {
        let h = harness(FakeRunner::default());
        let mut hinting = h.looping(fixed(test_config()));
        hinting.signals.started_at = Instant::now();
        let task = h.spawn(hinting);

        tokio::time::sleep(Duration::from_millis(150)).await;
        h.cancel.cancel();
        task.await.unwrap();

        assert_eq!(h.drains(), 0);
        assert!(h.observed.runs().iter().all(|run| *run == "skipped-quiet-after-start"));
        assert!(!h.observed.runs().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_in_place_checkpoint_suppresses_runs_then_opens_a_quiet_window() {
        let h = harness(FakeRunner::default());
        assert!(h.checkpoints.begin());
        let task = h.spawn(h.looping(fixed(test_config())));

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(h.drains(), 0);
        assert!(
            h.observed
                .runs()
                .iter()
                .all(|run| *run == "skipped-checkpoint-in-flight")
        );
        h.checkpoints.end();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(h.observed.count("skipped-quiet-after-checkpoint") > 0);
        assert_eq!(h.drains(), 0);
        *h.checkpoints.ended_at.lock() = Some(Instant::now() - Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(80)).await;
        h.cancel.cancel();
        task.await.unwrap();

        assert!(h.drains() >= 1, "runs resume once the quiet window has passed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_disable_parks_the_loop_and_an_enable_resumes_it() {
        let h = harness(FakeRunner::default());
        let off = Arc::new(AtomicBool::new(false));
        let task = h.spawn(h.looping(toggled(off.clone())));

        tokio::time::sleep(Duration::from_millis(100)).await;
        off.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let parked = h.drains();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.drains(), parked, "a parked loop runs nothing");
        off.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.cancel.cancel();
        task.await.unwrap();

        assert!(h.drains() > parked);
        assert_eq!(h.observed.count("skipped-disabled"), 1, "the disable is recorded once");
    }

    #[tokio::test(start_paused = true)]
    async fn a_loop_that_starts_disabled_joins_on_enable_without_a_record() {
        let h = harness(FakeRunner::default());
        let off = Arc::new(AtomicBool::new(true));
        let task = h.spawn(h.looping(toggled(off.clone())));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(h.observed.runs().is_empty());
        off.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.cancel.cancel();
        task.await.unwrap();

        assert!(h.drains() >= 1);
        assert_eq!(h.observed.count("skipped-disabled"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_balloon_gates_record_and_end_the_loop() {
        for (caps, outcome) in [
            (BalloonCaps::default(), "skipped-not-configured"),
            (
                BalloonCaps {
                    reporting: true,
                    hinting: true,
                },
                "skipped-reporting-active",
            ),
        ] {
            let h = harness(FakeRunner {
                caps: Mutex::new(VecDeque::from([Ok(caps)])),
                ..FakeRunner::default()
            });

            h.looping(fixed(test_config())).run().await;

            assert_eq!(h.observed.runs(), vec![outcome]);
            assert_eq!(h.drains(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_balloon_is_retried() {
        let h = harness(FakeRunner {
            caps: Mutex::new(VecDeque::from([
                Err("api busy".to_owned()),
                Err("api busy".to_owned()),
                Ok(BalloonCaps {
                    reporting: false,
                    hinting: true,
                }),
            ])),
            ..FakeRunner::default()
        });

        h.run_for(fixed(test_config()), Duration::from_millis(100)).await;

        let runs = h.observed.runs();
        assert_eq!(runs[..2], [BALLOON_UNKNOWN, BALLOON_UNKNOWN]);
        assert!(runs[2..].iter().all(|run| *run == "ok"));
        assert!(h.drains() >= 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_balloon_backs_off_after_the_fast_retries() {
        let h = harness(FakeRunner {
            caps: Mutex::new(VecDeque::from([Err("api busy".to_owned())])),
            ..FakeRunner::default()
        });

        h.run_for(fixed(test_config()), Duration::from_millis(395)).await;

        assert_eq!(h.observed.count(BALLOON_UNKNOWN), 10 + 10);
        assert_eq!(h.drains(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_old_firecracker_ends_the_loop_and_records_only_when_asked_for() {
        for (config, runs) in [
            (test_config(), vec!["skipped-fc-unsupported"]),
            (PeriodicConfig::default(), vec![]),
        ] {
            let h = harness(FakeRunner::default());
            let mut hinting = h.looping(fixed(config));
            hinting.gates.firecracker_supported = false;

            hinting.run().await;

            assert_eq!(h.observed.runs(), runs);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_ends_with_the_process() {
        let h = harness(scripted(|| Err(DrainFailure::Failed("connection refused".to_owned()))));
        h.runner.exited.store(true, Ordering::SeqCst);

        h.looping(fixed(test_config())).run().await;

        assert_eq!(h.drains(), 1);
        assert!(
            h.observed.runs().is_empty(),
            "a start failure of a gone process is not a hinting failure"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_loop_cancelled_before_the_gates_is_silent() {
        let h = harness(FakeRunner::default());
        h.cancel.cancel();

        h.looping(fixed(test_config())).run().await;

        assert!(h.observed.runs().is_empty());
        assert_eq!(h.drains(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unresponsive_guest_is_latched_backed_off_and_released() {
        let calls = Arc::new(AtomicU32::new(0));
        let recovered = Arc::new(AtomicBool::new(false));
        let (count, healed) = (calls.clone(), recovered.clone());
        let h = harness(scripted(move || {
            if healed.load(Ordering::SeqCst) {
                return Ok(());
            }
            let refused = DrainFailure::StartRefused {
                message: "balloon hinting start refused: 400".to_owned(),
                config_unknown: false,
            };
            let mut script = vec![Err(silent()), Err(silent()), Ok(()), Err(silent()), Err(refused)];
            let call = count.fetch_add(1, Ordering::SeqCst) as usize;
            match call < script.len() {
                true => script.swap_remove(call),
                false => Err(silent()),
            }
        }));
        let looping = h.looping(fixed(PeriodicConfig {
            unresponsive_retry: Duration::from_millis(200),
            ..test_config()
        }));
        let looping = Arc::new(looping);
        let running = looping.clone();
        let task = tokio::spawn(async move { running.run().await });

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            h.observed.runs()[..6],
            [
                "timeout-guest-silent",
                "timeout-guest-silent",
                "ok",
                "timeout-guest-silent",
                "refused",
                "timeout-guest-silent"
            ]
        );
        assert_eq!(*h.observed.latches.lock(), vec!["latched"]);
        assert!(looping.unresponsive.load(Ordering::SeqCst));
        assert_eq!(
            h.stops(),
            4,
            "every abandoned cycle was stopped, refusals have nothing to stop"
        );
        let latched_drains = h.drains();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let backed_off = h.drains() - latched_drains;
        assert!(
            (2..=3).contains(&backed_off),
            "attempts at the backed-off cadence: {backed_off}"
        );
        assert!(
            h.observed.count(LABEL_BACKOFF) == 0,
            "backoff ticks are labels, not runs"
        );
        recovered.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.cancel.cancel();
        task.await.unwrap();

        assert_eq!(*h.observed.latches.lock(), vec!["latched", "released"]);
        assert!(!looping.unresponsive.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_silent_runs_never_latches() {
        let h = harness(scripted(|| Err(silent())));

        h.run_for(
            fixed(PeriodicConfig {
                silent_runs: 0,
                ..test_config()
            }),
            Duration::from_millis(200),
        )
        .await;

        assert!(h.drains() >= 6);
        assert!(h.observed.latches.lock().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_latch_does_not_outlive_the_loop() {
        let h = harness(scripted(|| Err(silent())));

        h.run_for(fixed(test_config()), Duration::from_millis(200)).await;

        assert_eq!(*h.observed.latches.lock(), vec!["latched", "dropped"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_latched_loop_parks_within_one_interval_and_drops_the_latch() {
        let h = harness(scripted(|| Err(silent())));
        let off = Arc::new(AtomicBool::new(false));
        let task = h.spawn(h.looping(toggled(off.clone())));

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(*h.observed.latches.lock(), vec!["latched"]);
        off.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(*h.observed.latches.lock(), vec!["latched", "dropped"]);
        assert_eq!(h.observed.count("skipped-disabled"), 1);
        h.cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn engaged_failures_do_not_latch() {
        let h = harness(scripted(|| {
            Err(DrainFailure::InFlight {
                outcome: "timeout",
                message: "context deadline exceeded".to_owned(),
            })
        }));

        h.run_for(fixed(test_config()), Duration::from_millis(200)).await;

        assert!(h.drains() >= 6);
        assert!(h.observed.latches.lock().is_empty());
        assert!(h.observed.runs().iter().all(|run| *run == "timeout"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_balloon_without_hinting_ends_the_loop() {
        let h = harness(scripted(|| Err(DrainFailure::NotConfigured)));

        h.looping(fixed(test_config())).run().await;

        assert_eq!(h.observed.runs(), vec!["skipped-not-configured"]);
        assert_eq!(h.drains(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_stops_the_cycle_silently() {
        let h = harness(hanging());
        let task = h.spawn(h.looping(fixed(test_config())));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            h.hinting.runs.try_lock().is_err(),
            "a run in flight holds the run mutex"
        );
        h.cancel.cancel();
        task.await.unwrap();

        assert!(
            h.hinting.runs.try_lock().is_ok(),
            "the pause barrier passes once the cycle is stopped"
        );
        assert_eq!(h.stops(), 1);
        assert_eq!(*h.observed.stops.lock(), vec!["ok"]);
        assert!(h.observed.runs().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopping_sandbox_takes_its_cycle_with_it() {
        let h = harness(scripted(|| Err(silent())));
        h.hinting.stopping.store(true, Ordering::SeqCst);

        let attempt = h.looping(fixed(test_config())).hint_once(&test_config()).await;

        assert_eq!(attempt.result, HintResult::Aborted);
        assert_eq!(h.stops(), 0);
        assert_eq!(*h.observed.stops.lock(), vec!["skipped-stopping"]);
        assert!(h.observed.runs().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn start_failures_are_classified_like_go() {
        let refused = |config_unknown| DrainFailure::StartRefused {
            message: "balloon hinting start refused: 400".to_owned(),
            config_unknown,
        };
        let cases = [
            (refused(false), "refused", HintResult::GuestSilent),
            (refused(true), "failed", HintResult::Failed),
            (
                DrainFailure::Failed("start balloon hinting: 500".to_owned()),
                "failed",
                HintResult::Failed,
            ),
        ];
        for (failure, outcome, result) in cases {
            let failure = Mutex::new(Some(failure));
            let h = harness(scripted(move || Err(failure.lock().take().unwrap_or_else(silent))));

            let attempt = h.looping(fixed(test_config())).hint_once(&test_config()).await;

            assert_eq!((attempt.result, attempt.outcome), (result, Some(outcome)));
            assert_eq!(h.stops(), 0, "a start that never landed has nothing to stop");
        }
    }
}
