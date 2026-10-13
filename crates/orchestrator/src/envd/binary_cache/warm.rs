use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::Mutex;

use super::error::{Kind, WARM_OK, WARM_SUPERSEDED, WarmError, warm_result};
use super::identity::Identity;
use super::image::validate_image;
use super::scope::Scope;
use super::source::{BoundedError, LookupError};
use super::{Cache, Entry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlightError {
    Failed(WarmError),
    Panicked(String),
}

type FlightResult = Result<&'static str, FlightError>;
type Flight = Shared<BoxFuture<'static, FlightResult>>;

#[derive(Default)]
pub struct Flights {
    running: Arc<Mutex<HashMap<String, Flight>>>,
}

impl Flights {
    pub async fn run(&self, key: String, work: BoxFuture<'static, Result<&'static str, WarmError>>) -> FlightResult {
        let flight = {
            let mut running = self.running.lock();
            match running.get(&key) {
                Some(flight) => flight.clone(),
                None => {
                    let flight = self.start(key.clone(), work);
                    running.insert(key, flight.clone());
                    flight
                }
            }
        };
        flight.await
    }

    fn start(&self, key: String, work: BoxFuture<'static, Result<&'static str, WarmError>>) -> Flight {
        let running = self.running.clone();
        let task = tokio::spawn(async move {
            let outcome = AssertUnwindSafe(work).catch_unwind().await;
            running.lock().remove(&key);
            match outcome {
                Ok(result) => result.map_err(FlightError::Failed),
                Err(payload) => Err(FlightError::Panicked(panic_text(payload))),
            }
        });
        async move {
            task.await
                .unwrap_or_else(|join| Err(FlightError::Panicked(join.to_string())))
        }
        .boxed()
        .shared()
    }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
}

struct InFlightRelease {
    cache: Arc<Cache>,
    path: PathBuf,
}

impl Drop for InFlightRelease {
    fn drop(&mut self) {
        self.cache.release_in_flight(&self.path);
    }
}

impl Cache {
    pub async fn warm(self: &Arc<Self>, scope: &Scope, source: &Path) -> Result<(), WarmError> {
        self.warm_outcome(scope, source).await.map(|_| ())
    }

    pub(crate) async fn warm_outcome(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
    ) -> Result<&'static str, WarmError> {
        if self.schedule_suppressed(source) {
            return Err(WarmError::of(Kind::Suppressed));
        }
        let identity = match self.stat_source_bounded(scope, source).await {
            Ok(identity) => identity,
            Err(error) => return Err(self.stat_failed(source, &error)),
        };
        self.warm_with(scope, source, identity).await
    }

    fn stat_failed(&self, source: &Path, error: &BoundedError) -> WarmError {
        self.note_failure(source);
        WarmError::other(format!("stat envd binary {:?}: {error}", source.display()))
    }

    pub(crate) async fn warm_with(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
        identity: Identity,
    ) -> Result<&'static str, WarmError> {
        if self.schedule_suppressed(source) {
            return Err(WarmError::of(Kind::Suppressed));
        }
        if self.lookup_identity(source, identity).is_some() {
            return Ok(WARM_OK);
        }
        if self.identity_known_bad(source, identity) {
            return Err(WarmError::of(Kind::BadIdentityKnown));
        }
        let cache = self.clone();
        let (work_scope, work_source) = (scope.clone(), source.to_path_buf());
        let work = async move {
            if cache.lookup_identity(&work_source, identity).is_some() {
                return Ok(WARM_OK);
            }
            cache.copy_and_probe(&work_scope, &work_source, identity).await
        };
        match self.flights.run(identity.key(source), work.boxed()).await {
            Ok(result) => Ok(result),
            Err(FlightError::Failed(error)) => Err(error),
            Err(FlightError::Panicked(payload)) => {
                self.note_failure(source);
                tracing::error!(path = %source.display(), panic = %payload, "envd binary cache: warm panicked");
                Err(WarmError::tagged(
                    Kind::Panicked,
                    format!("{:?}: {payload}", source.display()),
                ))
            }
        }
    }

    pub(crate) async fn copy_and_probe(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
        identity: Identity,
    ) -> Result<&'static str, WarmError> {
        let local = match self.copy_in(scope, source, identity).await {
            Ok(local) => local,
            Err(error) if error.is(Kind::Superseded) => {
                self.note_success(source);
                return Ok(WARM_SUPERSEDED);
            }
            Err(error) => return Err(error),
        };
        let _until_published = InFlightRelease {
            cache: self.clone(),
            path: local.clone(),
        };
        self.validated(source, identity, &local).await?;
        let version = self.probed(scope, source, identity, &local).await?;
        self.published(scope, source, identity, local, version).await
    }

    async fn validated(self: &Arc<Self>, source: &Path, identity: Identity, local: &Path) -> Result<(), WarmError> {
        let (checked, size) = (local.to_path_buf(), identity.size);
        let verdict = tokio::task::spawn_blocking(move || validate_image(&checked, size))
            .await
            .map_err(|e| WarmError::other(format!("validate envd binary copy: {e}")))?;
        let Err(error) = verdict else {
            return Ok(());
        };
        let _ = std::fs::remove_file(local);
        if error.is(Kind::BadImage) {
            self.mark_bad_identity(source, identity);
            return Err(WarmError::wrapping(
                Kind::BadTarget,
                &format!("{:?}", source.display()),
                &error,
            ));
        }
        self.note_failure(source);
        Err(error)
    }

    async fn probed(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
        identity: Identity,
        local: &Path,
    ) -> Result<String, WarmError> {
        let failure = match self.probe.version(scope, local).await {
            Ok(version) if !version.is_empty() => return Ok(version),
            Ok(_) => super::probe::ProbeFailure::Other(format!(
                "probe of {:?} exited 0 without naming a version",
                local.display()
            )),
            Err(failure) => failure,
        };
        let _ = std::fs::remove_file(local);
        if failure.about_the_bytes(scope) {
            self.mark_bad_identity(source, identity);
            return Err(WarmError::wrapping(
                Kind::BadTarget,
                &format!("{:?}", source.display()),
                &WarmError::other(failure.to_string()),
            ));
        }
        self.note_failure(source);
        Err(WarmError::other(format!(
            "probe envd binary copy {:?}: {failure}",
            local.display()
        )))
    }

    async fn published(
        self: &Arc<Self>,
        scope: &Scope,
        source: &Path,
        identity: Identity,
        local: PathBuf,
        version: String,
    ) -> Result<&'static str, WarmError> {
        let entry = Entry::new(source.to_path_buf(), local, version, identity);
        match self.store(scope, entry).await {
            Ok(()) => {
                self.note_success(source);
                Ok(WARM_OK)
            }
            Err(error) if error.is(Kind::Superseded) => {
                self.note_success(source);
                Ok(WARM_SUPERSEDED)
            }
            Err(error) => {
                self.note_failure(source);
                Err(WarmError::context(
                    format!("publish envd binary copy {:?}", source.display()),
                    &error,
                ))
            }
        }
    }

    pub fn warm_async(self: &Arc<Self>, scope: &Scope, source: &Path) {
        self.warm_async_for(scope, source, None);
    }

    pub(crate) fn warm_async_for(self: &Arc<Self>, scope: &Scope, source: &Path, identity: Option<Identity>) {
        if self.dir.as_os_str().is_empty() || source.as_os_str().is_empty() {
            return;
        }
        let scope = scope.detached();
        if self.schedule_suppressed(source) {
            self.report_warm(source, Err(&WarmError::of(Kind::Suppressed)));
            return;
        }
        if let Some(identity) = identity
            && let Err(held) = self.begin_warm(source, identity)
        {
            self.report_warm(source, Err(&in_flight(held)));
            return;
        }
        let cache = self.clone();
        let source = source.to_path_buf();
        tokio::spawn(async move {
            let scope = scope.with_timeout(cache.warm_budget());
            cache.warm_in_background(&scope, &source, identity).await;
        });
    }

    async fn warm_in_background(self: &Arc<Self>, scope: &Scope, source: &Path, known: Option<Identity>) {
        let identity = match known {
            Some(identity) => identity,
            None => {
                let identity = match self.stat_source_bounded(scope, source).await {
                    Ok(identity) => identity,
                    Err(error) => {
                        self.note_failure(source);
                        let failed = WarmError::other(format!("stat envd binary {:?}: {error}", source.display()));
                        self.report_warm(source, Err(&failed));
                        return;
                    }
                };
                if let Err(held) = self.begin_warm(source, identity) {
                    self.report_warm(source, Err(&in_flight(held)));
                    return;
                }
                identity
            }
        };
        let result = self.warm_with(scope, source, identity).await;
        self.end_warm(source, identity);
        self.report_warm(source, result.as_ref().map(|result| *result));
    }

    pub(crate) fn report_warm(&self, source: &Path, outcome: Result<&str, &WarmError>) {
        let result = match outcome {
            Ok(result) => result,
            Err(error) => warm_result(error),
        };
        self.metrics.warm(result);
        let Err(error) = outcome else {
            return;
        };
        let path = source.display().to_string();
        match error {
            e if e.is(Kind::Suppressed) || e.is(Kind::BadIdentityKnown) || e.is(Kind::InFlight) => {
                tracing::debug!(path, result, error = %e, "envd binary cache: warm declined to try")
            }
            e if e.is(Kind::BadTarget) || self.should_warn(source) => tracing::warn!(
                path,
                result,
                error = %e,
                "envd binary cache: cannot warm; this node will not upgrade envd until it succeeds"
            ),
            e => tracing::debug!(path, result, error = %e, "envd binary cache: warm did not complete"),
        }
    }

    pub(crate) fn note_source_unreadable(&self, source: &Path, cause: &LookupError) {
        if self.dir.as_os_str().is_empty() || source.as_os_str().is_empty() {
            return;
        }
        if self.schedule_suppressed(source) {
            self.report_warm(source, Err(&WarmError::of(Kind::Suppressed)));
            return;
        }
        self.note_failure(source);
        self.report_warm(source, Err(&WarmError::other(cause.to_string())));
    }

    pub(crate) fn begin_warm(&self, source: &Path, identity: Identity) -> Result<(), Duration> {
        let key = identity.key(source);
        let mut state = self.state.lock();
        if let Some(claimed) = state.warming.get(&key) {
            return Err(claimed.elapsed());
        }
        state.warming.insert(key, Instant::now());
        Ok(())
    }

    pub(crate) fn end_warm(&self, source: &Path, identity: Identity) {
        self.state.lock().warming.remove(&identity.key(source));
    }
}

fn in_flight(held: Duration) -> WarmError {
    WarmError::tagged(
        Kind::InFlight,
        format!("held for {:?}", Duration::from_millis(held.as_millis() as u64)),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio_util::sync::CancellationToken;

    use super::super::error::{WARM_ALREADY_WARMING, WARM_FAILED, WARM_PINNED, WARM_SUPERSEDED};
    use super::super::metrics::WARMS_NAME;
    use super::super::metrics::testing::Collected;
    use super::super::probe::{FnProbe, Probe, ProbeFailure};
    use super::super::testing::*;
    use super::super::{BACKOFF_BASE, TEMP_PREFIX};
    use super::*;

    fn at(seconds: u64) -> std::time::SystemTime {
        mtime(1_700_000_000 + seconds)
    }

    fn cancelling_probe(token: CancellationToken, failure: ProbeFailure) -> Arc<dyn Probe> {
        FnProbe::shared(move |_scope, _path| {
            token.cancel();
            let failure = failure.clone();
            async move { Err(failure) }.boxed()
        })
    }

    fn failing_probe(failure: ProbeFailure) -> Arc<dyn Probe> {
        FnProbe::shared(move |_scope, _path| {
            let failure = failure.clone();
            async move { Err(failure) }.boxed()
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_verdict_about_the_bytes_pins_an_identity() {
        let signalled = |name: &str| {
            let status = std::process::Command::new("/bin/sh")
                .args(["-c", &format!("kill -{name} $$")])
                .status()
                .unwrap();
            ProbeFailure::from_status(status)
        };
        let exited = ProbeFailure::from_status(
            std::process::Command::new("/bin/sh")
                .args(["-c", "exit 3"])
                .status()
                .unwrap(),
        );
        let cases: Vec<(&str, ProbeFailure, bool, bool)> = vec![
            ("the loader refusing the image is pinned", not_executable(), false, true),
            (
                "a probe stopped by the warm's own deadline is not",
                ProbeFailure::Other("signal: killed".to_owned()),
                true,
                false,
            ),
            ("a child that ran and exited nonzero is not", exited, false, false),
            (
                "a copy that vanished under the probe is not",
                ProbeFailure::Other("fork/exec: no such file".to_owned()),
                false,
                false,
            ),
            (
                "fd exhaustion allocating the probe's pipes is not",
                ProbeFailure::Other("pipe2: too many open files".to_owned()),
                false,
                false,
            ),
            (
                "a packed node failing fork is not",
                ProbeFailure::Other("fork/exec: cannot allocate memory".to_owned()),
                false,
                false,
            ),
            (
                "a cache directory mounted noexec is not",
                ProbeFailure::Other("fork/exec: permission denied".to_owned()),
                false,
                false,
            ),
            (
                "an image that faults on its own instructions is pinned",
                signalled("SEGV"),
                false,
                true,
            ),
            ("an image killed by SIGBUS is pinned", signalled("BUS"), false, true),
            (
                "a child killed by an unrelated signal is not",
                signalled("TERM"),
                false,
                false,
            ),
        ];
        for (name, failure, cancels, pins) in cases {
            let (scope, token) = Scope::cancellable();
            let probe = match cancels {
                true => cancelling_probe(token, failure),
                false => failing_probe(failure),
            };
            let rig = Rig::with(probe);
            write_source(&rig.src, "binary-v1", at(0));

            let error = rig.cache.warm(&scope, &rig.src).await.unwrap_err();

            let identity = real_stat(&rig.src).unwrap();
            assert_eq!(rig.cache.identity_known_bad(&rig.src, identity), pins, "{name}");
            assert_eq!(error.is(Kind::BadTarget), pins, "{name}: {error}");
            if !pins {
                rig.cache.note_success(&rig.src);
                let retried = rig.cache.warm(&background(), &rig.src).await;
                assert!(
                    !retried.is_err_and(|e| e.is(Kind::BadIdentityKnown)),
                    "{name}: the identity must still be retried"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_target_on_one_path_does_not_starve_another() {
        let probe = CountingProbe::new("0.7.0");
        let rig = Rig::with(probe.probe());
        let bad = rig.dir.join("envd.bad");
        let good = rig.dir.join("envd");
        write_source(&bad, "unrunnable", at(0));
        write_source(&good, "binary-v1", at(0));
        let bad_copy = rig.cache_dir().join(identity_of(&bad).local_name(&bad));
        probe.fail_path(bad_copy);

        let error = rig.cache.warm(&background(), &bad).await.unwrap_err();
        assert!(error.is(Kind::BadTarget));

        for _ in 0..3 {
            rig.cache
                .warm(&background(), &good)
                .await
                .expect("a bad target on one path must not suppress warms of another");
        }
        assert_eq!(rig.cache.lookup(&good).await.unwrap().version, "0.7.0");
    }

    fn identity_of(path: &Path) -> Identity {
        real_stat(path).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_inside_its_schedule_is_not_stated_on_the_source() {
        let stats = Arc::new(AtomicUsize::new(0));
        let counted = stats.clone();
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .stat_source(FnStat::shared(move |path| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    real_stat(path)
                }))
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        rig.cache.note_failure(&rig.src);
        let before = stats.load(Ordering::SeqCst);

        let error = rig.cache.warm(&background(), &rig.src).await.unwrap_err();
        assert!(error.is(Kind::Suppressed));
        assert_eq!(
            before,
            stats.load(Ordering::SeqCst),
            "a suppressed path must not be stat-ed on the source"
        );

        rig.cache.warm_async(&background(), &rig.src);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(stats.load(Ordering::SeqCst), before, "nor by the background warmer");

        rig.cache.note_success(&rig.src);
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        assert!(
            stats.load(Ordering::SeqCst) > before,
            "an unsuppressed warm must stat the source"
        );
    }

    fn wedging_cache(base: &Path, gate: &Arc<Gate>, entered: &Arc<AtomicUsize>, budget: Duration) -> Cache {
        let (gate, entered) = (gate.clone(), entered.clone());
        Cache::builder()
            .dir(base.join("cache"))
            .probe(ok_probe("0.7.0"))
            .budget(budget)
            .stat_source(FnStat::shared(move |_path| {
                entered.fetch_add(1, Ordering::SeqCst);
                gate.wait();
                Err(std::io::Error::other("unreachable"))
            }))
            .build()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wedged_source_stat_does_not_retain_the_publisher() {
        let gate = Gate::closed();
        let entered = Arc::new(AtomicUsize::new(0));
        let rig = Rig::with_cache(|base| wedging_cache(base, &gate, &entered, Duration::ZERO));
        let _unwedge = OnDrop::run({
            let gate = gate.clone();
            move || gate.open()
        });
        write_source(&rig.src, "binary-v1", at(0));
        let (scope, cancel) = Scope::cancellable();
        let entry = Entry::new(
            rig.src.clone(),
            rig.dir.join("cache").join("copy"),
            "0.7.0".to_owned(),
            Identity {
                size: 1,
                modified: at(0),
            },
        );

        let publishing = {
            let cache = rig.cache.clone();
            tokio::spawn(async move { cache.store(&scope, entry).await })
        };
        assert!(eventually(|| entered.load(Ordering::SeqCst) > 0, Duration::from_secs(5)).await);
        cancel.cancel();

        let declined = tokio::time::timeout(Duration::from_secs(10), publishing)
            .await
            .expect("publication was retained by a wedged source stat; the warming slot would leak")
            .unwrap();
        assert!(declined.is_err());
        rig.cache.replace_source(Arc::new(super::super::source::Filesystem));
        assert!(rig.cache.lookup(&rig.src).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wedged_source_stat_does_not_retain_the_warm() {
        let gate = Gate::closed();
        let entered = Arc::new(AtomicUsize::new(0));
        let rig = Rig::with_cache(|base| wedging_cache(base, &gate, &entered, Duration::ZERO));
        let _unwedge = OnDrop::run({
            let gate = gate.clone();
            move || gate.open()
        });
        write_source(&rig.src, "binary-v1", at(0));
        let (scope, cancel) = Scope::cancellable();

        let warming = {
            let (cache, source) = (rig.cache.clone(), rig.src.clone());
            tokio::spawn(async move { cache.warm(&scope, &source).await })
        };
        assert!(eventually(|| entered.load(Ordering::SeqCst) > 0, Duration::from_secs(5)).await);
        cancel.cancel();

        let error = tokio::time::timeout(Duration::from_secs(10), warming)
            .await
            .expect("Warm was retained by a wedged source stat; the warming slot would leak")
            .unwrap();
        assert!(error.is_err(), "the warm must fail rather than hang");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unconfirmable_publish_is_a_failure_and_not_a_supersede() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .stat_source(FnStat::shared(move |path| {
                    match counted.fetch_add(1, Ordering::SeqCst) {
                        0 => real_stat(path),
                        _ => Err(std::io::Error::other("mount did not answer")),
                    }
                }))
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));

        let error = rig.cache.warm(&background(), &rig.src).await.unwrap_err();

        assert!(!error.is(Kind::Superseded), "a mount failure is not a promotion");
        assert_eq!(rig.cache.state.lock().backoff[&rig.src].failures, 1);
        assert!(rig.cache.schedule_suppressed(&rig.src));

        rig.cache.note_success(&rig.src);
        calls.store(0, Ordering::SeqCst);
        let source = rig.src.clone();
        let promoted = Arc::new(AtomicUsize::new(0));
        let counted = promoted.clone();
        rig.cache.replace_source(FnStat::shared(move |path| {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                return real_stat(path);
            }
            write_source(&source, "binary-v2", at(500));
            real_stat(path)
        }));
        rig.cache
            .warm(&background(), &rig.src)
            .await
            .expect("a supersede is not an error");
        assert!(
            !rig.cache.state.lock().backoff.contains_key(&rig.src),
            "a supersede must not schedule a retry"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_source_stat_inside_the_warming_slot_can_wedge_it() {
        let gate = Gate::closed();
        let cache_dir = Arc::new(Mutex::new(PathBuf::new()));
        let rig = Rig::with_cache(|base| {
            *cache_dir.lock() = base.join("cache");
            let (gate, watched) = (gate.clone(), cache_dir.clone());
            Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .budget(Duration::from_millis(200))
                .stat_source(FnStat::shared(move |path| {
                    let copied = std::fs::read_dir(&*watched.lock()).is_ok_and(|names| {
                        names
                            .filter_map(Result::ok)
                            .any(|name| !name.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
                    });
                    if copied {
                        gate.wait();
                        return Err(std::io::Error::other("wedged mount"));
                    }
                    real_stat(path)
                }))
                .build()
        });
        let _unwedge = OnDrop::run({
            let gate = gate.clone();
            move || gate.open()
        });
        write_source(&rig.src, "binary-v1", at(0));

        rig.cache.warm_async(&background(), &rig.src);

        assert!(
            eventually(
                || std::fs::read_dir(rig.cache_dir()).is_ok_and(|names| names
                    .filter_map(Result::ok)
                    .any(|name| !name.file_name().to_string_lossy().starts_with(TEMP_PREFIX))),
                Duration::from_secs(10)
            )
            .await,
            "the warm never copied, so this test would not reach the step under test"
        );
        let identity = real_stat(&rig.src).unwrap();
        assert!(
            eventually(
                || rig.cache.begin_warm(&rig.src, identity).is_ok(),
                Duration::from_secs(10)
            )
            .await,
            "the warming slot was never released; these bytes could never warm again"
        );
        rig.cache.end_warm(&rig.src, identity);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_truncated_image_is_pinned_without_being_executed() {
        let probe = CountingProbe::new("0.7.0");
        let rig = Rig::with(probe.probe());
        write_raw(&rig.src, &elf_with_payload("the-whole-binary")[..20], at(0));

        let error = rig.cache.warm(&background(), &rig.src).await.unwrap_err();

        assert!(error.is(Kind::BadTarget) && error.is(Kind::BadImage), "{error}");
        assert_eq!(
            probe.count(),
            0,
            "an incomplete image must never be executed to be judged"
        );
        assert!(rig.cache.identity_known_bad(&rig.src, real_stat(&rig.src).unwrap()));

        write_source(&rig.src, "the-whole-binary", at(100));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        assert!(probe.count() > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_copy_the_source_outran_is_superseded_and_not_a_bad_target() {
        let probe = CountingProbe::new("0.7.0");
        let full = elf_with_payload("generation-one");
        let calls = Arc::new(AtomicUsize::new(0));
        let rig = Rig::with_cache(|base| {
            let (counted, source, size) = (calls.clone(), base.join("envd"), full.len() as u64);
            Cache::builder()
                .dir(base.join("cache"))
                .probe(probe.probe())
                .stat_source(FnStat::shared(move |path| {
                    if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                        return real_stat(path).map(|identity| Identity { size, ..identity });
                    }
                    write_source(&source, "generation-two", at(500));
                    real_stat(path)
                }))
                .build()
        });
        write_raw(&rig.src, &full[..20], at(0));

        let result = rig.cache.warm_outcome(&background(), &rig.src).await;

        assert_eq!(result, Ok(WARM_SUPERSEDED), "a promotion mid-copy is not a failure");
        assert_eq!(probe.count(), 0, "an abandoned copy must not be probed either");
        assert!(!rig.cache.identity_known_bad(&rig.src, real_stat(&rig.src).unwrap()));
        assert!(
            !rig.cache.state.lock().backoff.contains_key(&rig.src),
            "nor scheduled: nothing failed"
        );

        rig.cache.warm(&background(), &rig.src).await.unwrap();
        assert_eq!(rig.cache.lookup(&rig.src).await.unwrap().version, "0.7.0");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_stat_failure_schedules_like_any_other() {
        let stats = Arc::new(AtomicUsize::new(0));
        let counted = stats.clone();
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .stat_source(FnStat::shared(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Err(std::io::Error::other("mount did not answer its metadata"))
                }))
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));

        assert!(rig.cache.warm(&background(), &rig.src).await.is_err());

        assert_eq!(rig.cache.state.lock().backoff[&rig.src].failures, 1);
        assert!(rig.cache.schedule_suppressed(&rig.src));
        let before = stats.load(Ordering::SeqCst);
        let suppressed = rig.cache.warm(&background(), &rig.src).await.unwrap_err();
        assert!(suppressed.is(Kind::Suppressed));
        assert_eq!(
            before,
            stats.load(Ordering::SeqCst),
            "a scheduled path must not re-stat the source"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_warm_stuck_on_one_identity_does_not_hold_out_the_next() {
        let wedged = Arc::new(tokio::sync::Notify::new());
        let probe = {
            let wedged = wedged.clone();
            FnProbe::shared(move |scope, path| {
                let wedged = wedged.clone();
                let inner = content_probe();
                async move {
                    let version = inner.version(&scope, &path).await?;
                    if version == "v1" {
                        wedged.notified().await;
                        return Err(ProbeFailure::Other("context canceled".to_owned()));
                    }
                    Ok(version)
                }
                .boxed()
            })
        };
        let rig = Rig::with(probe);
        let _unwedge = OnDrop::run({
            let wedged = wedged.clone();
            move || wedged.notify_waiters()
        });
        write_generation(&rig.src, 1);
        let first = real_stat(&rig.src).unwrap();
        rig.cache.warm_async_for(&background(), &rig.src, Some(first));
        assert!(
            eventually(
                || rig.cache.begin_warm(&rig.src, first).is_err(),
                Duration::from_secs(10)
            )
            .await,
            "the first warm never claimed its slot, so nothing is being held out"
        );

        write_generation(&rig.src, 2);
        rig.cache
            .warm_async_for(&background(), &rig.src, Some(real_stat(&rig.src).unwrap()));

        assert!(
            eventually(
                || rig
                    .cache
                    .lookup_identity(&rig.src, real_stat(&rig.src).unwrap())
                    .is_some_and(|entry| entry.version == "v2"),
                Duration::from_secs(30)
            )
            .await,
            "a promotion must warm while an earlier generation's warm is still wedged"
        );
        wedged.notify_waiters();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_warm_refused_because_one_is_in_flight_is_counted() {
        let collected = Collected::new();
        let release = Arc::new(tokio::sync::Notify::new());
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let probe = {
            let release = release.clone();
            FnProbe::shared(move |_scope, _path| {
                let (release, entered) = (release.clone(), entered_tx.clone());
                async move {
                    let _ = entered.try_send(());
                    release.notified().await;
                    Ok("0.7.0".to_owned())
                }
                .boxed()
            })
        };
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(probe)
                .metrics(collected.metrics.clone())
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        let identity = real_stat(&rig.src).unwrap();
        rig.cache.warm_async_for(&background(), &rig.src, Some(identity));
        entered_rx.recv().await;

        for _ in 0..8 {
            rig.cache.warm_async_for(&background(), &rig.src, Some(identity));
        }

        assert_eq!(
            collected
                .by_attribute(WARMS_NAME, "result")
                .get(WARM_ALREADY_WARMING)
                .copied()
                .unwrap_or(0),
            8,
            "a warm refused because one is already in flight must be counted, not dropped"
        );
        assert_eq!(
            rig.cache.state.lock().warming.len(),
            1,
            "a refused warm must not claim a slot of its own"
        );
        release.notify_one();
        assert!(
            eventually(
                || rig.cache.lookup_identity(&rig.src, identity).is_some(),
                Duration::from_secs(10)
            )
            .await
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pinned_identity_is_reported_as_pinned_and_not_as_suppression() {
        let collected = Collected::new();
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(failing_probe(not_executable()))
                .metrics(collected.metrics.clone())
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        let error = rig.cache.warm(&background(), &rig.src).await.unwrap_err();
        assert!(error.is(Kind::BadTarget));

        rig.cache
            .warm_async_for(&background(), &rig.src, Some(real_stat(&rig.src).unwrap()));

        assert!(
            eventually(
                || collected
                    .by_attribute(WARMS_NAME, "result")
                    .get(WARM_PINNED)
                    .copied()
                    .unwrap_or(0)
                    >= 1,
                Duration::from_secs(10)
            )
            .await,
            "a refusal to re-copy known-bad bytes must report pinned"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panic_inside_a_warm_is_contained_and_scheduled() {
        let probe = FnProbe::shared(|_scope, _path| {
            async move {
                if true {
                    panic!("a parser that does not return");
                }
                Ok(String::new())
            }
            .boxed()
        });
        let rig = Rig::with(probe);
        write_source(&rig.src, "binary-v1", at(0));

        let error = rig.cache.warm(&background(), &rig.src).await.unwrap_err();

        assert!(
            error.is(Kind::Panicked),
            "a panic must come back as an error, not as a dead process"
        );
        assert_eq!(super::super::error::warm_result(&error), WARM_FAILED);
        assert!(
            !rig.cache.identity_known_bad(&rig.src, real_stat(&rig.src).unwrap()),
            "a panic must never pin an identity"
        );
        assert!(
            rig.cache.schedule_suppressed(&rig.src),
            "a panic must arm the retry schedule"
        );
    }

    #[test]
    fn the_first_retry_after_a_failure_waits_the_base() {
        assert_eq!(BACKOFF_BASE, Duration::from_secs(1));
    }
}
