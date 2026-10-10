use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing::Instrument;

use super::metrics::{Op, Outcome};
use super::probe::{ExecProbe, Probe, ProbeFailure};
use super::scope::Scope;
use super::source::{LookupError, bounded_stat_without_cache};
use super::{Cache, Entry};
use crate::envd::upgrade::{SourceError, UpgradeSource};

pub const REASON_NOT_CACHED: &str = "binary_not_cached";
pub const REASON_COPY_VANISHED: &str = "copy_vanished";

const MISS_NOT_WARMED: &str = "not_warmed";
const MISS_STALLED: &str = "stalled";
const MISS_CANCELLED: &str = "cancelled";
const MISS_UNREADABLE: &str = "unreadable";
const MISS_NO_CACHE: &str = "no_cache";

pub fn gated_reason(reason: &str, outcome: Option<Outcome>) -> String {
    match (reason, outcome) {
        ("getversion_failed", Some(Outcome::Miss)) => REASON_NOT_CACHED.to_owned(),
        _ => reason.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotCached {
    pub stalled: bool,
    message: String,
}

impl NotCached {
    fn plain() -> Self {
        Self {
            stalled: false,
            message: "envd binary not cached on this node".to_owned(),
        }
    }

    fn because(cause: &LookupError, stalled: bool) -> Self {
        Self {
            stalled,
            message: format!("envd binary not cached on this node: {cause}"),
        }
    }
}

impl fmt::Display for NotCached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NotCached {}

#[derive(Default)]
struct Resolved {
    entry: Option<Arc<Entry>>,
    outcome: Option<Outcome>,
    source_unreadable: bool,
}

pub struct Resolver {
    cache: Arc<Cache>,
    op: Op,
    scope: Scope,
    resolved: Mutex<Resolved>,
}

impl Resolver {
    pub fn new(cache: Arc<Cache>, op: Op) -> Self {
        Self::within(cache, op, Scope::background())
    }

    pub fn within(cache: Arc<Cache>, op: Op, scope: Scope) -> Self {
        Self {
            cache,
            op,
            scope,
            resolved: Mutex::default(),
        }
    }

    pub async fn version(&self, scope: &Scope, path: &Path) -> Result<String, NotCached> {
        let span = tracing::debug_span!(
            "envd-binary-cache-lookup",
            op = self.op.as_str(),
            path = %path.display()
        );
        self.lookup_version(scope, path).instrument(span).await
    }

    async fn lookup_version(&self, scope: &Scope, path: &Path) -> Result<String, NotCached> {
        let looked = self.cache.lookup_source(scope, path).await;
        if let Ok((Some(entry), _)) = &looked {
            self.settle(Some(entry.clone()), Outcome::Hit, None);
            return Ok(entry.version.clone());
        }
        let (error, cause) = self.miss(scope, path, looked);
        self.settle(None, Outcome::Miss, Some(cause));
        Err(error)
    }

    fn miss(
        &self,
        scope: &Scope,
        path: &Path,
        looked: Result<(Option<Arc<Entry>>, super::identity::Identity), LookupError>,
    ) -> (NotCached, &'static str) {
        match looked {
            Err(error) if scope.error().is_some() || error.is_stalled() => {
                let cause = match scope.error() {
                    Some(_) => MISS_CANCELLED,
                    None => {
                        self.cache.note_source_stalled(path, &error);
                        MISS_STALLED
                    }
                };
                (NotCached::because(&error, true), cause)
            }
            Err(error) if error.is_no_cache() => (NotCached::plain(), MISS_NO_CACHE),
            Err(error) => {
                self.cache.note_source_unreadable(path, &error);
                self.resolved.lock().source_unreadable = true;
                (NotCached::plain(), MISS_UNREADABLE)
            }
            Ok((_, identity)) => {
                self.cache.warm_async_for(scope, path, Some(identity));
                (NotCached::plain(), MISS_NOT_WARMED)
            }
        }
    }

    fn settle(&self, entry: Option<Arc<Entry>>, outcome: Outcome, cause: Option<&str>) {
        {
            let mut resolved = self.resolved.lock();
            resolved.entry = entry;
            resolved.outcome = Some(outcome);
        }
        self.cache.metrics.read(self.op, outcome, cause);
    }

    pub async fn stat(&self, scope: &Scope, path: &Path) -> Result<(), LookupError> {
        let stat = self.cache.stat_source_for_lookup(scope, path, true).await;
        let Err(error) = &stat else {
            return Ok(());
        };
        if scope.error().is_some() || error.is_stalled() {
            let cause = match scope.error() {
                Some(_) => MISS_CANCELLED,
                None => {
                    self.cache.note_source_stalled(path, error);
                    MISS_STALLED
                }
            };
            self.settle(None, Outcome::Miss, Some(cause));
        }
        stat.map(|_| ())
    }

    pub fn entry_for(&self, path: &Path) -> Option<Arc<Entry>> {
        self.resolved
            .lock()
            .entry
            .clone()
            .filter(|entry| entry.source_path == path)
    }

    pub fn outcome(&self) -> Option<Outcome> {
        self.resolved.lock().outcome
    }

    pub fn deferral_outcome(&self) -> Option<Outcome> {
        let resolved = self.resolved.lock();
        match resolved.source_unreadable {
            true => None,
            false => resolved.outcome,
        }
    }

    pub async fn source_path(&self, source: &Path) -> Option<PathBuf> {
        let delivered = self
            .entry_for(source)
            .map(|entry| entry.local_path.clone())
            .filter(|local| std::fs::metadata(local).is_ok());
        let outcome = match delivered {
            Some(_) => Outcome::Copy,
            None => Outcome::Stale,
        };
        self.cache.metrics.delivery(self.op, outcome);
        delivered
    }
}

pub async fn source_path(resolver: Option<&Resolver>, source: &Path) -> Option<PathBuf> {
    match resolver {
        Some(resolver) => resolver.source_path(source).await,
        None => Some(source.to_path_buf()),
    }
}

pub fn deferral_outcome(resolver: Option<&Resolver>) -> Option<Outcome> {
    resolver.and_then(Resolver::deferral_outcome)
}

pub fn outcome(resolver: Option<&Resolver>) -> Option<Outcome> {
    resolver.and_then(Resolver::outcome)
}

pub fn entry_for(resolver: Option<&Resolver>, path: &Path) -> Option<Arc<Entry>> {
    resolver.and_then(|resolver| resolver.entry_for(path))
}

pub async fn stat(resolver: Option<&Resolver>, scope: &Scope, path: &Path) -> Result<(), LookupError> {
    match resolver {
        Some(resolver) => resolver.stat(scope, path).await,
        None => bounded_stat_without_cache(scope, path).await.map(|_| ()),
    }
}

fn stat_error(error: LookupError) -> SourceError {
    match error.scope_error().is_some() || error.is_stalled() {
        true => SourceError::Stalled(error.to_string()),
        false => SourceError::Failed(error.to_string()),
    }
}

impl UpgradeSource for Resolver {
    async fn stat(&self, path: &Path) -> Result<(), SourceError> {
        Resolver::stat(self, &self.scope, path).await.map_err(stat_error)
    }

    async fn version(&self, path: &Path) -> Result<String, SourceError> {
        Resolver::version(self, &self.scope, path)
            .await
            .map_err(|error| match error.stalled {
                true => SourceError::Stalled(error.to_string()),
                false => SourceError::Failed(error.to_string()),
            })
    }
}

pub struct DirectSource {
    cache: Option<Arc<Cache>>,
    scope: Scope,
}

impl DirectSource {
    pub fn new(cache: Option<Arc<Cache>>, scope: Scope) -> Self {
        Self { cache, scope }
    }
}

impl UpgradeSource for DirectSource {
    async fn stat(&self, path: &Path) -> Result<(), SourceError> {
        let stat = match &self.cache {
            Some(cache) => cache.bounded_stat(&self.scope, path).await,
            None => bounded_stat_without_cache(&self.scope, path).await,
        };
        stat.map(|_| ()).map_err(stat_error)
    }

    async fn version(&self, path: &Path) -> Result<String, SourceError> {
        ExecProbe
            .version(&self.scope, path)
            .await
            .map_err(|failure| match self.scope.error() {
                Some(_) => SourceError::Stalled(failure_text(&failure)),
                None => SourceError::Failed(format!("error while getting envd version: {}", failure_text(&failure))),
            })
    }
}

fn failure_text(failure: &ProbeFailure) -> String {
    failure.to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::super::error::WARM_FAILED;
    use super::super::metrics::testing::Collected;
    use super::super::metrics::{READS_NAME, WARMS_NAME};
    use super::super::source::LookupKind;
    use super::super::testing::*;
    use super::*;
    use crate::envd::upgrade::{Decline, resolve};

    fn at(seconds: u64) -> std::time::SystemTime {
        mtime(1_700_000_000 + seconds)
    }

    fn resolver(rig: &Rig, op: Op) -> Resolver {
        Resolver::new(rig.cache.clone(), op)
    }

    async fn warmed(version: &str) -> (Rig, Arc<CountingProbe>) {
        let probe = CountingProbe::new(version);
        let rig = Rig::with(probe.probe());
        write_source(&rig.src, "binary-v1", at(0));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        (rig, probe)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hit_is_served_from_the_cache() {
        let (rig, probe) = warmed("0.7.0").await;
        let probes_after_warm = probe.count();
        let resolver = resolver(&rig, Op::Live);

        let version = resolver.version(&background(), &rig.src).await.unwrap();

        assert_eq!(version, "0.7.0");
        assert_eq!(resolver.outcome(), Some(Outcome::Hit));
        assert_eq!(probe.count(), probes_after_warm, "a hit must not probe anything");
        let entry = resolver.entry_for(&rig.src).unwrap();
        assert_eq!(
            version, entry.version,
            "the delivered bytes must be the ones whose version was reported"
        );
        let local = resolver.source_path(&rig.src).await.unwrap();
        assert_eq!(local, entry.local_path);
        assert_ne!(local, rig.src);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_entry_for_a_different_path_is_refused() {
        let (rig, _) = warmed("0.7.0").await;
        let other = rig.dir.join("envd.abcdef0");
        write_source(&other, "binary-v2", at(500));
        let resolver = resolver(&rig, Op::Live);
        resolver.version(&background(), &rig.src).await.unwrap();

        assert!(resolver.entry_for(&other).is_none());
        assert!(
            resolver.source_path(&other).await.is_none(),
            "an engaged resolver must refuse rather than read the source"
        );
        assert!(resolver.entry_for(&rig.src).is_some());
        let probed = resolver.source_path(&rig.src).await.unwrap();
        assert_ne!(probed, rig.src, "the resolved path is served from the copy");
    }

    #[tokio::test]
    async fn a_missing_resolver_behaves_as_the_uncached_path() {
        let path = Path::new("/fc-envd/envd");

        assert_eq!(source_path(None, path).await, Some(path.to_path_buf()));
        assert!(entry_for(None, path).is_none());
        assert_eq!(outcome(None), None);
        assert_eq!(deferral_outcome(None), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unusable_cache_defers_every_upgrade() {
        let probe = CountingProbe::new("0.7.0");
        let rig = Rig::with_cache(|base| {
            std::fs::write(base.join("blocker"), "x").unwrap();
            Cache::builder()
                .dir(base.join("blocker").join("cache"))
                .probe(probe.probe())
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        let resolver = resolver(&rig, Op::Live);

        let error = resolver.version(&background(), &rig.src).await.unwrap_err();

        assert!(!error.stalled);
        assert_eq!(resolver.outcome(), Some(Outcome::Miss));
        assert!(
            !probe.paths().contains(&rig.src),
            "a deferral must not probe the source"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_miss_defers_rather_than_reading_the_source() {
        let probe = CountingProbe::new("0.7.0");
        let rig = Rig::with(probe.probe());
        write_source(&rig.src, "binary-v1", at(0));
        let resolver = resolver(&rig, Op::Live);

        let error = resolver.version(&background(), &rig.src).await.unwrap_err();

        assert_eq!(error.to_string(), "envd binary not cached on this node");
        assert_eq!(resolver.outcome(), Some(Outcome::Miss));
        assert!(
            !probe.paths().contains(&rig.src),
            "the source must not be probed on a miss"
        );
        assert!(
            resolver.entry_for(&rig.src).is_none(),
            "a deferral resolves nothing to deliver"
        );

        assert!(
            eventually(
                || rig
                    .cache
                    .lookup_identity(&rig.src, real_stat(&rig.src).unwrap())
                    .is_some(),
                Duration::from_secs(10)
            )
            .await,
            "the miss must warm the cache in the background"
        );
        let next = self::resolver(&rig, Op::Live);
        assert_eq!(next.version(&background(), &rig.src).await.unwrap(), "0.7.0");
        assert_eq!(next.outcome(), Some(Outcome::Hit));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_vanished_copy_is_refused_and_the_source_is_not_the_fallback() {
        let (rig, _) = warmed("0.7.0").await;
        let resolver = resolver(&rig, Op::Live);
        resolver.version(&background(), &rig.src).await.unwrap();
        let entry = resolver.entry_for(&rig.src).unwrap();
        std::fs::remove_file(&entry.local_path).unwrap();

        let path = resolver.source_path(&rig.src).await;

        assert!(
            path.is_none(),
            "a vanished copy must be refused, not read from the mount instead"
        );
        assert!(
            rig.src.exists(),
            "the source is intact; it is deliberately not the fallback"
        );
    }

    #[test]
    fn the_gated_reason_separates_a_deferral_from_a_failed_probe() {
        for (name, reason, outcome, expected) in [
            (
                "a deferral is relabelled",
                "getversion_failed",
                Some(Outcome::Miss),
                REASON_NOT_CACHED,
            ),
            (
                "a genuine probe failure is not",
                "getversion_failed",
                Some(Outcome::Hit),
                "getversion_failed",
            ),
            (
                "flag off passes through untouched",
                "getversion_failed",
                None,
                "getversion_failed",
            ),
            (
                "other reasons are never rewritten",
                "not_staged",
                Some(Outcome::Miss),
                "not_staged",
            ),
            (
                "downgrade is never rewritten",
                "downgrade",
                Some(Outcome::Miss),
                "downgrade",
            ),
            ("an empty reason stays empty", "", Some(Outcome::Miss), ""),
        ] {
            assert_eq!(gated_reason(reason, outcome), expected, "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lookup_whose_source_stat_failed_schedules_instead_of_warming() {
        let collected = Collected::new();
        let stats = Arc::new(AtomicUsize::new(0));
        let counted = stats.clone();
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .metrics(collected.metrics.clone())
                .stat_source(FnStat::shared(move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Err(std::io::Error::other("mount is not answering"))
                }))
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        let resolver = resolver(&rig, Op::Live);

        let error = resolver.version(&background(), &rig.src).await.unwrap_err();

        assert!(!error.stalled, "an unreadable source is a miss, and the upgrade defers");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            stats.load(Ordering::SeqCst),
            1,
            "a failed source stat must not be re-taken by a warm"
        );
        assert_eq!(collected.by_attribute(WARMS_NAME, "result").get(WARM_FAILED), Some(&1));
        assert!(
            rig.cache.schedule_suppressed(&rig.src),
            "the failure must arm the retry schedule"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_an_unwarmed_cache_is_relabeled_as_a_deferral() {
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .stat_source(FnStat::shared(|_| Err(std::io::Error::other("mount is not answering"))))
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        let unreadable = resolver(&rig, Op::Live);
        unreadable.version(&background(), &rig.src).await.unwrap_err();
        assert_eq!(
            unreadable.outcome(),
            Some(Outcome::Miss),
            "the read outcome is still a miss: this resume read nothing from the cache"
        );
        assert_eq!(
            gated_reason("getversion_failed", unreadable.deferral_outcome()),
            "getversion_failed",
            "an unreadable target must stay an operator error the upgrade logs"
        );

        let warm = Rig::with(ok_probe("0.7.0"));
        write_source(&warm.src, "binary-v1", at(0));
        let unwarmed = resolver(&warm, Op::Live);
        unwarmed.version(&background(), &warm.src).await.unwrap_err();
        assert_eq!(unwarmed.outcome(), Some(Outcome::Miss));
        assert_eq!(
            gated_reason("getversion_failed", unwarmed.deferral_outcome()),
            REASON_NOT_CACHED,
            "a readable source that is merely not warmed yet is the deferral this label is for"
        );
    }

    fn stalled_rig(budget: Duration, collected: Option<&Collected>) -> (Rig, Arc<StalledStat>) {
        let stalled = StalledStat::new();
        let source = stalled.source();
        let metrics = collected.map(|collected| collected.metrics.clone());
        let rig = Rig::with_cache(move |base| {
            let builder = Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .lookup_budget(budget)
                .stat_source(source);
            match metrics {
                Some(metrics) => builder.metrics(metrics).build(),
                None => builder.build(),
            }
        });
        write_source(&rig.src, "binary-v1", at(0));
        (rig, stalled)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lookup_does_not_wait_on_a_stalled_source_stat() {
        let budget = Duration::from_millis(100);
        let (rig, stalled) = stalled_rig(budget, None);
        let resolver = resolver(&rig, Op::Offline);
        let started = std::time::Instant::now();

        let error = resolver.version(&background(), &rig.src).await.unwrap_err();

        let elapsed = started.elapsed();
        assert!(error.stalled, "a stalled stat is a miss, and the upgrade defers");
        assert!(elapsed >= budget, "the lookup waits out its budget before giving up");
        assert!(elapsed < Duration::from_secs(5), "and not the stall");
        assert_eq!(resolver.deferral_outcome(), Some(Outcome::Miss));
        assert_eq!(
            gated_reason("getversion_failed", resolver.deferral_outcome()),
            REASON_NOT_CACHED
        );
        assert!(
            !rig.cache.schedule_suppressed(&rig.src),
            "a stall must not arm the retry schedule"
        );
        assert_eq!(stalled.calls(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lookups_share_one_stalled_stat_and_fail_fast_once_it_outlives_the_budget() {
        let budget = Duration::from_millis(300);
        let (rig, stalled) = stalled_rig(budget, None);

        let resumes: Vec<_> = (0..16)
            .map(|_| {
                let (cache, source) = (rig.cache.clone(), rig.src.clone());
                tokio::spawn(async move {
                    Resolver::new(cache, Op::Live)
                        .version(&background(), &source)
                        .await
                        .unwrap_err()
                })
            })
            .collect();
        for resume in resumes {
            resume.await.unwrap();
        }
        assert_eq!(
            stalled.calls(),
            1,
            "concurrent lookups must share one stat, not stack one task each"
        );

        let started = std::time::Instant::now();
        Resolver::new(rig.cache.clone(), Op::Live)
            .version(&background(), &rig.src)
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < budget / 2,
            "a stat already past its budget must fail the lookup at once"
        );
        assert_eq!(stalled.calls(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stat_that_lands_after_its_lookups_gave_up_warms_the_cache() {
        let (rig, stalled) = stalled_rig(Duration::from_millis(50), None);
        Resolver::new(rig.cache.clone(), Op::Live)
            .version(&background(), &rig.src)
            .await
            .unwrap_err();

        stalled.unblock();

        assert!(
            eventually(
                || {
                    let state = rig.cache.state.lock();
                    state.stat_flights.is_empty() && state.entries.len() == 1
                },
                Duration::from_secs(10)
            )
            .await,
            "the late stat must retire its flight and warm the cache"
        );
        let cache = Arc::new(Cache::builder().dir(rig.cache_dir()).probe(ok_probe("0.7.0")).build());
        drop(cache);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_the_stall_the_next_resume_takes_its_own_stat_and_hits() {
        let stalled = StalledStat::new();
        let source = stalled.source();
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(ok_probe("0.7.0"))
                .lookup_budget(Duration::from_millis(50))
                .stat_source(source)
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        Resolver::new(rig.cache.clone(), Op::Live)
            .version(&background(), &rig.src)
            .await
            .unwrap_err();
        stalled.unblock();
        assert!(eventually(|| rig.cache.state.lock().entries.len() == 1, Duration::from_secs(10)).await);

        let verdict = Resolver::new(rig.cache.clone(), Op::Live);
        let version = verdict.version(&Scope::background(), &rig.src).await;

        assert_eq!(version.as_deref(), Ok("0.7.0"));
        assert_eq!(verdict.outcome(), Some(Outcome::Hit));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stat_and_a_lookup_racing_the_budget_cannot_both_skip_the_warm() {
        let rig = Rig::with(ok_probe("0.7.0"));
        let identity = real_stat(&std::env::temp_dir()).unwrap();
        let path = Path::new("/src");

        let landed_first = Arc::new(super::super::source::Flight::new());
        assert!(
            !rig.cache.land_flight(path, &landed_first, Ok(identity)),
            "no lookup had given up"
        );
        assert_eq!(
            rig.cache.abandon_flight(&landed_first, true),
            Some(Ok(identity)),
            "a lookup timing out after the stat landed must use its result"
        );

        let abandoned_first = Arc::new(super::super::source::Flight::new());
        assert_eq!(rig.cache.abandon_flight(&abandoned_first, true), None);
        assert!(
            rig.cache.land_flight(path, &abandoned_first, Ok(identity)),
            "the stat must see the abandonment and start the warm"
        );

        let cache_off = Arc::new(super::super::source::Flight::new());
        assert_eq!(rig.cache.abandon_flight(&cache_off, false), None);
        assert!(
            !rig.cache.land_flight(path, &cache_off, Ok(identity)),
            "a bounded stat for a cache-off caller must not warm the cache"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_upgrade_resolution_does_not_hang_on_a_stalled_candidate_stat() {
        for target in ["promoted", "v0.9.0"] {
            let (rig, stalled) = stalled_rig(Duration::from_millis(100), None);
            let resolver = resolver(&rig, Op::Offline);
            let started = std::time::Instant::now();

            let resolved = resolve(&resolver, target, "0.6.0", &rig.src).await;

            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the resolution must not wait out the stall"
            );
            assert_eq!(resolved, Err(Decline::SourceStalled), "{target}");
            assert_eq!(stalled.calls(), 1, "one stat reached the mount");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_resolution_counts_one_stalled_read() {
        let collected = Collected::new();
        let (rig, _stalled) = stalled_rig(Duration::from_millis(50), Some(&collected));
        let resolver = resolver(&rig, Op::Live);

        let resolved = resolve(&resolver, "promoted", "0.6.0", &rig.src).await;

        assert_eq!(resolved, Err(Decline::SourceStalled));
        assert_eq!(resolver.outcome(), Some(Outcome::Miss));
        let causes = collected.by_attribute(READS_NAME, "cause");
        assert_eq!(
            causes,
            std::collections::BTreeMap::from([("stalled".to_owned(), 1)]),
            "a stalled resolution is one read, a miss caused by the stall"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_candidate_check_warms_the_cache_when_it_lands() {
        let (rig, stalled) = stalled_rig(Duration::from_millis(50), None);
        let first = resolver(&rig, Op::Live);
        assert_eq!(
            resolve(&first, "promoted", "0.6.0", &rig.src).await,
            Err(Decline::SourceStalled)
        );

        stalled.unblock();
        assert!(
            eventually(|| rig.cache.state.lock().entries.len() == 1, Duration::from_secs(10)).await,
            "the late candidate stat must warm the cache"
        );

        let next = resolver(&rig, Op::Live);
        let upgrade = resolve(&next, "promoted", "0.6.0", &rig.src).await.unwrap();
        assert_eq!(upgrade.path, rig.src);
        assert_eq!(upgrade.version, "0.7.0");
        assert_eq!(next.outcome(), Some(Outcome::Hit));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bounded_stat_for_a_cache_off_caller_never_warms() {
        let (rig, stalled) = stalled_rig(Duration::from_millis(50), None);

        let error = rig.cache.bounded_stat(&background(), &rig.src).await.unwrap_err();
        assert!(error.is_stalled());

        stalled.unblock();
        assert!(
            eventually(
                || rig.cache.state.lock().stat_flights.is_empty(),
                Duration::from_secs(10)
            )
            .await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let state = rig.cache.state.lock();
        assert!(
            state.entries.is_empty() && state.warming.is_empty(),
            "a cache-off caller's stat must not warm the cache"
        );
    }

    #[tokio::test]
    async fn a_missing_cache_still_bounds_the_stat() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("envd");
        std::fs::write(&source, "x").unwrap();

        assert!(stat(None, &background(), &source).await.is_ok());

        let missing = stat(None, &background(), &directory.path().join("missing"))
            .await
            .unwrap_err();
        assert!(matches!(&missing.kind, LookupKind::Stat(failure) if failure.not_found()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lookup_returns_when_its_scope_is_cancelled() {
        let (rig, _stalled) = stalled_rig(Duration::from_secs(60), None);
        let (scope, cancel) = Scope::cancellable();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();

        let error = rig.cache.lookup_source(&scope, &rig.src).await.unwrap_err();

        assert_eq!(error.scope_error(), Some(super::super::scope::ScopeError::Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a cancelled resume must not wait on the stall"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_lookup_does_not_count_against_the_source() {
        let (rig, _stalled) = stalled_rig(Duration::from_secs(60), None);
        let (scope, cancel) = Scope::cancellable();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let resolver = resolver(&rig, Op::Live);

        let error = resolver.version(&scope, &rig.src).await.unwrap_err();

        assert!(
            error.stalled,
            "a cancelled resume reads as the stall the resolver maps to source_stalled"
        );
        assert_eq!(
            resolver.deferral_outcome(),
            Some(Outcome::Miss),
            "a cancelled lookup defers like any other miss"
        );
        assert!(
            !rig.cache.schedule_suppressed(&rig.src),
            "a cancelled caller must not arm the source's retry schedule"
        );
    }
}
