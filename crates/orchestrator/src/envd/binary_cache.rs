pub mod copy;
pub mod error;
pub mod identity;
pub mod image;
pub mod metrics;
pub mod probe;
pub mod resolver;
pub mod schedule;
pub mod scope;
pub mod source;
pub mod store;
#[cfg(test)]
pub mod testing;
pub mod warm;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};

use self::identity::Identity;
use self::metrics::CacheMetrics;
use self::probe::Probe;
use self::schedule::PathBackoff;
use self::scope::Scope;
use self::source::{Filesystem, Flight, SourceStat};
use self::warm::Flights;

pub const MAX_ENTRIES: usize = 4;
pub const TEMP_PREFIX: &str = ".tmp-";
pub const WARM_BUDGET: Duration = Duration::from_secs(30);
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
pub const BACKOFF_CEILING: Duration = Duration::from_secs(60);
pub const COPY_CHUNK: usize = 1 << 20;
pub const LOOKUP_STAT_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct Entry {
    pub source_path: PathBuf,
    pub local_path: PathBuf,
    pub version: String,
    identity: Identity,
}

impl Entry {
    pub fn new(source_path: PathBuf, local_path: PathBuf, version: String, identity: Identity) -> Self {
        Self {
            source_path,
            local_path,
            version,
            identity,
        }
    }

    pub fn cached(&self) -> bool {
        self.local_path != self.source_path
    }
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) entries: HashMap<PathBuf, Arc<Entry>>,
    pub(crate) order: Vec<PathBuf>,
    pub(crate) in_flight: HashMap<PathBuf, usize>,
    pub(crate) warming: HashMap<String, std::time::Instant>,
    pub(crate) backoff: HashMap<PathBuf, PathBackoff>,
    pub(crate) stat_flights: HashMap<PathBuf, Arc<Flight>>,
}

pub struct Cache {
    dir: PathBuf,
    probe: Arc<dyn Probe>,
    budget: Duration,
    lookup_budget: Duration,
    stat_source: RwLock<Arc<dyn SourceStat>>,
    metrics: Arc<CacheMetrics>,
    reap_temps: Once,
    state: Mutex<State>,
    flights: Flights,
}

#[bon::bon]
impl Cache {
    #[builder]
    pub fn new(
        #[builder(into)] dir: PathBuf,
        probe: Arc<dyn Probe>,
        #[builder(default = CacheMetrics::global())] metrics: Arc<CacheMetrics>,
        #[builder(default)] budget: Duration,
        #[builder(default)] lookup_budget: Duration,
        #[builder(default = Arc::new(Filesystem))] stat_source: Arc<dyn SourceStat>,
    ) -> Self {
        Self {
            dir,
            probe,
            budget,
            lookup_budget,
            stat_source: RwLock::new(stat_source),
            metrics,
            reap_temps: Once::new(),
            state: Mutex::default(),
            flights: Flights::default(),
        }
    }
}

impl Cache {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn metrics(&self) -> &Arc<CacheMetrics> {
        &self.metrics
    }

    pub async fn lookup(self: &Arc<Self>, source: &Path) -> Option<Arc<Entry>> {
        self.lookup_source(&Scope::background(), source)
            .await
            .ok()
            .and_then(|(entry, _)| entry)
    }

    fn warm_budget(&self) -> Duration {
        match self.budget.is_zero() {
            true => WARM_BUDGET,
            false => self.budget,
        }
    }

    fn lookup_stat_budget(&self) -> Duration {
        match self.lookup_budget.is_zero() {
            true => LOOKUP_STAT_BUDGET,
            false => self.lookup_budget,
        }
    }

    pub(crate) fn lookup_identity(&self, source: &Path, identity: Identity) -> Option<Arc<Entry>> {
        let mut state = self.state.lock();
        let entry = state.entries.get(source)?.clone();
        if entry.identity != identity {
            return None;
        }
        if entry.cached() && std::fs::metadata(&entry.local_path).is_err() {
            state.entries.remove(source);
            state.order.retain(|path| path != source);
            return None;
        }
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::FutureExt;

    use super::error::{Kind, warm_result};
    use super::probe::FnProbe;
    use super::testing::*;
    use super::*;

    fn rig(version: &str) -> (Rig, Arc<CountingProbe>) {
        let probe = CountingProbe::new(version);
        (Rig::with(probe.probe()), probe)
    }

    fn at(seconds: u64) -> std::time::SystemTime {
        mtime(1_700_000_000 + seconds)
    }

    fn entries_in(directory: &Path) -> Vec<String> {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lookup_misses_until_warmed() {
        let (rig, probe) = rig("0.7.0");
        write_source(&rig.src, "binary-v1", at(0));

        assert!(
            rig.cache.lookup(&rig.src).await.is_none(),
            "nothing is cached before a warm"
        );
        assert_eq!(probe.count(), 0, "a lookup must not probe");

        rig.cache.warm(&background(), &rig.src).await.unwrap();

        let entry = rig.cache.lookup(&rig.src).await.unwrap();
        assert_eq!(entry.version, "0.7.0");
        assert_eq!(probe.count(), 1);
        assert!(entry.cached());
        assert_eq!(entry.local_path.parent().unwrap(), rig.cache_dir());
        assert_eq!(payload_of(&entry.local_path), "binary-v1");
        let mode = std::fs::metadata(&entry.local_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let again = rig.cache.lookup(&rig.src).await.unwrap();
        assert!(Arc::ptr_eq(&entry, &again));
        assert_eq!(probe.count(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn warming_is_idempotent() {
        let (rig, probe) = rig("0.7.0");
        write_source(&rig.src, "binary-v1", at(0));

        for _ in 0..3 {
            rig.cache.warm(&background(), &rig.src).await.unwrap();
        }

        assert_eq!(probe.count(), 1, "warming an already-cached binary must be free");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_warm_probes_the_copy_and_not_the_source() {
        let (rig, probe) = rig("0.7.0");
        write_source(&rig.src, "binary-v1", at(0));
        rig.cache.warm(&background(), &rig.src).await.unwrap();

        let entry = rig.cache.lookup(&rig.src).await.unwrap();

        assert_eq!(probe.paths(), vec![entry.local_path.clone()]);
        assert!(!probe.paths().contains(&rig.src));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_advanced_mtime_alone_invalidates_at_equal_size() {
        let (rig, probe) = rig("0.7.0");
        write_source(&rig.src, "AAAAAAAAA", at(0));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let first = rig.cache.lookup(&rig.src).await.unwrap();

        write_source(&rig.src, "BBBBBBBBB", at(500));
        probe.set_version("0.7.1");

        assert!(
            rig.cache.lookup(&rig.src).await.is_none(),
            "an advanced mtime must invalidate the entry"
        );
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let second = rig.cache.lookup(&rig.src).await.unwrap();
        assert_eq!(second.version, "0.7.1");
        assert_eq!(probe.count(), 2);
        assert_eq!(payload_of(&second.local_path), "BBBBBBBBB");
        assert!(!first.local_path.exists(), "the superseded copy is gone");
        assert!(second.local_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_size_invalidates_on_its_own() {
        let (rig, _) = rig("0.7.0");
        write_source(&rig.src, "short", at(0));
        rig.cache.warm(&background(), &rig.src).await.unwrap();

        write_source(&rig.src, "considerably-longer", at(0));

        assert!(
            rig.cache.lookup(&rig.src).await.is_none(),
            "a changed size must invalidate the entry"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_probe_publishes_nothing_and_pins_the_identity() {
        let (rig, probe) = rig("0.7.0");
        write_source(&rig.src, "binary-v1", at(0));
        probe.fail_once();

        let error = rig.cache.warm(&background(), &rig.src).await.unwrap_err();
        assert!(
            error.is(Kind::BadTarget),
            "a copy that will not run is a property of the target, not of this node"
        );
        assert!(rig.cache.lookup(&rig.src).await.is_none());

        let probes_after_failure = probe.count();
        let again = rig.cache.warm(&background(), &rig.src).await.unwrap_err();
        assert!(
            again.is(Kind::BadIdentityKnown),
            "an identity already proved unable to run must not be copied again"
        );
        assert_eq!(
            probe.count(),
            probes_after_failure,
            "no second probe of the same identity"
        );

        write_source(&rig.src, "binary-v1", at(100));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let entry = rig.cache.lookup(&rig.src).await.unwrap();
        assert_eq!(entry.version, "0.7.0");

        let names = entries_in(&rig.cache_dir());
        assert_eq!(
            names,
            vec![entry.local_path.file_name().unwrap().to_string_lossy().into_owned()]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_source_is_an_error_and_is_not_probed() {
        let (rig, probe) = rig("0.7.0");

        let error = rig.cache.warm(&background(), &rig.dir.join("absent")).await;

        assert!(error.is_err());
        assert_eq!(probe.count(), 0, "an absent source must not be probed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unusable_cache_fails_the_warm_and_probes_nothing() {
        let probe = CountingProbe::new("0.7.0");
        let rig = Rig::with_cache(|base| {
            let blocker = base.join("blocker");
            std::fs::write(&blocker, "x").unwrap();
            Cache::builder().dir(blocker.join("cache")).probe(probe.probe()).build()
        });
        write_source(&rig.src, "binary-v1", at(0));

        assert!(rig.cache.warm(&background(), &rig.src).await.is_err());

        assert!(rig.cache.lookup(&rig.src).await.is_none());
        assert_eq!(probe.count(), 0, "an unusable cache must not probe anything");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_cache_touches_no_filesystem() {
        let base = tempfile::tempdir().unwrap();
        let cache_dir = base.path().join("cache");

        let _configured = Cache::builder().dir(&cache_dir).probe(ok_probe("0.7.0")).build();
        let _unconfigured = Cache::builder().dir("").probe(ok_probe("0.7.0")).build();

        assert!(!cache_dir.exists());
        assert_eq!(entries_in(base.path()), Vec::<String>::new());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn warming_with_no_directory_configured_does_nothing() {
        let probe = CountingProbe::new("0.7.0");
        let rig = Rig::with_cache(|_| Cache::builder().dir("").probe(probe.probe()).build());
        write_source(&rig.src, "binary-v1", at(0));

        rig.cache.warm_async(&background(), &rig.src);

        assert!(rig.cache.lookup(&rig.src).await.is_none());
        assert_eq!(probe.count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_warms_collapse_into_one_read() {
        let probes = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let (entered_tx, entered_rx) = tokio::sync::mpsc::channel::<()>(1);
        let probe = {
            let (probes, release) = (probes.clone(), release.clone());
            FnProbe::shared(move |_scope, _path| {
                probes.fetch_add(1, Ordering::SeqCst);
                let (release, entered) = (release.clone(), entered_tx.clone());
                async move {
                    let _ = entered.try_send(());
                    release.notified().await;
                    Ok("0.7.0".to_owned())
                }
                .boxed()
            })
        };
        let rig = Rig::with(probe);
        write_source(&rig.src, "binary-v1", at(0));

        let callers: Vec<_> = (0..16)
            .map(|_| {
                let (cache, source) = (rig.cache.clone(), rig.src.clone());
                tokio::spawn(async move { drop(cache.warm(&background(), &source).await) })
            })
            .collect();
        let mut entered_rx = entered_rx;
        entered_rx.recv().await;
        release.notify_waiters();
        release.notify_one();
        for caller in callers {
            caller.await.unwrap();
        }

        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "concurrent warms must collapse into one read"
        );
        assert!(rig.cache.lookup(&rig.src).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_oldest_entry_is_evicted_past_the_cap_along_with_its_copy() {
        let (rig, _) = rig("0.7.0");
        let mut paths = Vec::new();
        let mut locals = Vec::new();
        for index in 0..=MAX_ENTRIES {
            let path = rig.dir.join(format!("envd.{}", (b'a' + index as u8) as char));
            write_source(&path, &format!("binary-{index}"), at(index as u64));
            rig.cache.warm(&background(), &path).await.unwrap();
            locals.push(rig.cache.lookup(&path).await.unwrap().local_path.clone());
            paths.push(path);
        }

        assert!(
            !locals[0].exists(),
            "the first inserted is evicted and its copy deleted"
        );
        assert!(locals[1..].iter().all(|local| local.exists()));
        assert_eq!(entries_in(&rig.cache_dir()).len(), MAX_ENTRIES);
        assert!(
            rig.cache.lookup(&paths[0]).await.is_none(),
            "the evicted path is a miss, not a ghost"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sweep_removes_orphans_but_spares_live_and_in_progress_files() {
        let (rig, _) = rig("0.7.0");
        write_source(&rig.src, "binary-v1", at(0));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let first = rig.cache.lookup(&rig.src).await.unwrap();
        let orphan = rig.cache_dir().join("envd.999.999.deadbeef");
        let in_progress = rig.cache_dir().join(format!("{TEMP_PREFIX}concurrent"));
        std::fs::write(&orphan, "stale").unwrap();
        std::fs::write(&in_progress, "partial").unwrap();
        let second_source = rig.dir.join("envd.abc");
        write_source(&second_source, "binary-v2", at(500));

        rig.cache.warm(&background(), &second_source).await.unwrap();

        let second = rig.cache.lookup(&second_source).await.unwrap();
        assert!(!orphan.exists(), "an unreferenced copy must be reclaimed");
        assert!(in_progress.exists(), "an in-progress temporary file must be spared");
        assert!(first.local_path.exists() && second.local_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_use_reaps_temporary_files_from_a_previous_process() {
        let (rig, _) = rig("0.7.0");
        std::fs::create_dir_all(rig.cache_dir()).unwrap();
        let abandoned = rig.cache_dir().join(format!("{TEMP_PREFIX}abandoned"));
        std::fs::write(&abandoned, "partial").unwrap();
        write_source(&rig.src, "binary-v1", at(0));

        rig.cache.warm(&background(), &rig.src).await.unwrap();

        let entry = rig.cache.lookup(&rig.src).await.unwrap();
        assert!(!abandoned.exists());
        assert!(entry.local_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_vanished_copy_reads_as_a_miss() {
        let (rig, probe) = rig("0.7.0");
        write_source(&rig.src, "binary-v1", at(0));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let first = rig.cache.lookup(&rig.src).await.unwrap();
        std::fs::remove_file(&first.local_path).unwrap();

        assert!(
            rig.cache.lookup(&rig.src).await.is_none(),
            "a missing copy must read as a miss, not a hit"
        );

        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let second = rig.cache.lookup(&rig.src).await.unwrap();
        assert!(second.local_path.exists());
        assert_eq!(probe.count(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cache_side_failure_suppresses_the_next_copy_until_a_success_clears_it() {
        let probe = CountingProbe::new("0.7.0");
        let rig = Rig::with_cache(|base| {
            std::fs::write(base.join("blocker"), "x").unwrap();
            Cache::builder()
                .dir(base.join("blocker").join("cache"))
                .probe(probe.probe())
                .build()
        });
        write_source(&rig.src, "binary-v1", at(0));
        assert!(rig.cache.warm(&background(), &rig.src).await.is_err());

        std::fs::remove_file(rig.dir.join("blocker")).unwrap();
        std::fs::create_dir_all(rig.dir.join("blocker").join("cache")).unwrap();

        let suppressed = rig.cache.warm(&background(), &rig.src).await.unwrap_err();
        assert!(
            suppressed.is(Kind::Suppressed),
            "the schedule must suppress the next attempt"
        );

        rig.cache.note_success(&rig.src);

        rig.cache
            .warm(&background(), &rig.src)
            .await
            .expect("a success must clear the schedule, not disable the cache");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_side_failure_schedules_from_the_base_and_does_not_pin() {
        let (rig, _) = rig("0.7.0");
        std::fs::create_dir_all(&rig.src).unwrap();

        assert!(rig.cache.warm(&background(), &rig.src).await.is_err());

        let (failures, bad, until) = {
            let state = rig.cache.state.lock();
            let backoff = state.backoff.get(&rig.src).expect("a source failure must be scheduled");
            (backoff.failures, backoff.bad, backoff.until.unwrap())
        };
        assert_eq!(failures, 1);
        assert!(bad.is_none(), "a source failure says nothing about the bytes");
        let wait = until.saturating_duration_since(std::time::Instant::now());
        assert!(
            wait <= BACKOFF_BASE,
            "the first retry waits the base, not the ceiling: {wait:?}"
        );

        rig.cache.note_success(&rig.src);
        std::fs::remove_dir_all(&rig.src).unwrap();
        write_source(&rig.src, "binary-v1", at(0));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        assert!(rig.cache.lookup(&rig.src).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_warm_finishing_last_does_not_replace_the_newer_generation() {
        let (rig, _) = rig("0.7.1");
        write_source(&rig.src, "BBBBBBBBB", at(500));
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let newer = rig.cache.lookup(&rig.src).await.unwrap();
        let stale_path = rig.cache_dir().join("envd.9.1700000000000000000.stale");
        std::fs::write(&stale_path, "AAAAAAAAA").unwrap();

        let stale = Entry::new(
            rig.src.clone(),
            stale_path.clone(),
            "0.7.0".to_owned(),
            identity::Identity {
                size: 9,
                modified: at(0),
            },
        );
        let _ = rig.cache.store(&background(), stale).await;

        let current = rig
            .cache
            .lookup(&rig.src)
            .await
            .expect("a stale warm must not leave an entry that no longer matches");
        assert_eq!(current.version, "0.7.1");
        assert_eq!(current.local_path, newer.local_path);
        assert!(newer.local_path.exists());
        assert!(!stale_path.exists(), "the loser cleans up after itself");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_warm_runs_per_binary_however_often_it_is_asked_for() {
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
        let rig = Rig::with(probe);
        write_source(&rig.src, "binary-v1", at(0));

        for _ in 0..32 {
            rig.cache.warm_async(&background(), &rig.src);
        }
        entered_rx.recv().await;

        assert_eq!(
            rig.cache.state.lock().warming.len(),
            1,
            "a second warm for the same binary must not spawn"
        );
        release.notify_one();
        assert!(
            eventually(
                || rig
                    .cache
                    .lookup_identity(&rig.src, real_stat(&rig.src).unwrap())
                    .is_some(),
                Duration::from_secs(5)
            )
            .await
        );
        assert!(
            eventually(|| rig.cache.state.lock().warming.is_empty(), Duration::from_secs(5)).await,
            "the claim must be released when the warm finishes"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_entry_with_a_future_mtime_does_not_wedge_the_cache() {
        let (rig, _) = rig("0.7.0");
        write_source(&rig.src, "binary-v1", at(0));
        std::fs::create_dir_all(rig.cache_dir()).unwrap();
        let poisoned = rig.cache_dir().join("envd.9.9999999999999999999.poisoned");
        std::fs::write(&poisoned, "stale").unwrap();

        let entry = Entry::new(
            rig.src.clone(),
            poisoned,
            "9.9.9".to_owned(),
            identity::Identity {
                size: 9,
                modified: mtime(4_000_000_000),
            },
        );
        let _ = rig.cache.store(&background(), entry).await;

        assert!(
            rig.cache.lookup(&rig.src).await.is_none(),
            "an entry that does not describe the source must not be resident"
        );
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let entry = rig.cache.lookup(&rig.src).await.expect("the cache must not be wedged");
        assert_eq!(entry.version, "0.7.0");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_short_read_of_an_unchanged_source_is_a_copy_failure() {
        let probe = ok_probe("0.7.0");
        let rig = Rig::with_cache(|base| {
            Cache::builder()
                .dir(base.join("cache"))
                .probe(probe)
                .stat_source(FnStat::shared(|path| {
                    real_stat(path).map(|identity| identity::Identity {
                        size: identity.size + 1,
                        ..identity
                    })
                }))
                .build()
        });
        write_source(&rig.src, "the-whole-binary", at(0));

        let error = rig.cache.warm(&background(), &rig.src).await.unwrap_err();

        assert!(error.to_string().contains("short read"), "{error}");
        let real = real_stat(&rig.src).unwrap();
        assert!(
            !rig.cache.identity_known_bad(&rig.src, real),
            "a short read is the copy's fault, never a verdict about the binary"
        );
        assert_eq!(rig.cache.state.lock().backoff[&rig.src].failures, 1);

        rig.cache.replace_source(Arc::new(source::Filesystem));
        rig.cache.note_success(&rig.src);
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        assert!(rig.cache.lookup(&rig.src).await.is_some());
    }

    fn assert_directory_holds_exactly_the_live_copies(cache: &Cache) {
        let state = cache.state.lock();
        let live: std::collections::HashSet<PathBuf> =
            state.entries.values().map(|entry| entry.local_path.clone()).collect();
        assert!(state.entries.len() <= MAX_ENTRIES, "entries must never exceed the cap");
        assert!(
            state.in_flight.is_empty(),
            "no copy may still be in flight once quiesced"
        );
        assert!(state.warming.is_empty(), "no warm may still be claimed once quiesced");
        for name in std::fs::read_dir(&cache.dir).unwrap() {
            let name = name.unwrap();
            if name.file_type().unwrap().is_dir() || name.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
                continue;
            }
            assert!(
                live.contains(&cache.dir.join(name.file_name())),
                "the directory holds a copy no entry references"
            );
        }
        live.iter()
            .for_each(|copy| assert!(copy.exists(), "a live entry's copy is missing"));
    }

    async fn quiesced(cache: &Cache) {
        assert!(
            eventually(
                || {
                    let state = cache.state.lock();
                    state.warming.is_empty() && state.in_flight.is_empty()
                },
                Duration::from_secs(30)
            )
            .await,
            "warms must all retire"
        );
    }

    fn promoting(
        source: PathBuf,
        first: u64,
        pause: Duration,
    ) -> (Arc<std::sync::atomic::AtomicBool>, tokio::task::JoinHandle<u64>) {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let mutator = tokio::spawn(async move {
            let mut promotions = 0;
            let mut generation = first;
            while !stopped.load(Ordering::SeqCst) {
                write_generation(&source, generation);
                promotions += 1;
                generation += 1;
                tokio::time::sleep(pause).await;
            }
            promotions
        });
        (stop, mutator)
    }

    type Violations = Arc<Mutex<Vec<String>>>;

    fn record(violations: &Violations, message: String) {
        let mut messages = violations.lock();
        if messages.len() < 20 {
            messages.push(message);
        }
    }

    fn payload_trimmed(path: &Path) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        Some(
            String::from_utf8_lossy(bytes.get(ELF_HEADER_LEN..).unwrap_or(&bytes))
                .trim()
                .to_owned(),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_lookups_and_warms_under_promotion_churn() {
        let rig = Rig::with(content_probe());
        write_generation(&rig.src, 0);
        let (stop, mutator) = promoting(rig.src.clone(), 1, Duration::from_millis(2));
        let violations: Violations = Arc::default();
        let (hits, misses) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));

        let readers: Vec<_> = (0..24)
            .map(|_| {
                let (cache, source, violations) = (rig.cache.clone(), rig.src.clone(), violations.clone());
                let (hits, misses) = (hits.clone(), misses.clone());
                tokio::spawn(async move {
                    for _ in 0..150 {
                        let mut entry = cache.lookup(&source).await;
                        if entry.is_none() {
                            misses.fetch_add(1, Ordering::SeqCst);
                            if cache.warm(&background(), &source).await.is_ok() {
                                entry = cache.lookup(&source).await;
                            }
                        }
                        let Some(entry) = entry else { continue };
                        hits.fetch_add(1, Ordering::SeqCst);
                        match payload_trimmed(&entry.local_path) {
                            Some(payload) if payload != entry.version => record(
                                &violations,
                                format!(
                                    "copy {} holds {payload:?} but its entry records {:?}",
                                    entry.local_path.display(),
                                    entry.version
                                ),
                            ),
                            _ => {}
                        }
                    }
                })
            })
            .collect();
        for reader in readers {
            reader.await.unwrap();
        }
        assert!(violations.lock().is_empty(), "{:?}", violations.lock());
        stop.store(true, Ordering::SeqCst);
        let promotions = mutator.await.unwrap();
        quiesced(&rig.cache).await;
        assert_directory_holds_exactly_the_live_copies(&rig.cache);
        assert!(promotions > 0, "the mutator must have promoted at least once");
        assert!(misses.load(Ordering::SeqCst) > 0, "churn must have produced misses");

        let settled_hits = Arc::new(AtomicUsize::new(0));
        let settled: Vec<_> = (0..24)
            .map(|_| {
                let (cache, source, violations, settled_hits) = (
                    rig.cache.clone(),
                    rig.src.clone(),
                    violations.clone(),
                    settled_hits.clone(),
                );
                tokio::spawn(async move {
                    for _ in 0..150 {
                        let mut entry = cache.lookup(&source).await;
                        if entry.is_none() {
                            if cache.warm(&background(), &source).await.is_err() {
                                continue;
                            }
                            entry = cache.lookup(&source).await;
                        }
                        let Some(entry) = entry else { continue };
                        settled_hits.fetch_add(1, Ordering::SeqCst);
                        match payload_trimmed(&entry.local_path) {
                            Some(payload) if payload == entry.version => {}
                            other => record(
                                &violations,
                                format!(
                                    "stable source, copy {} holds {other:?} but entry records {:?}",
                                    entry.local_path.display(),
                                    entry.version
                                ),
                            ),
                        }
                    }
                })
            })
            .collect();
        for reader in settled {
            reader.await.unwrap();
        }
        assert!(violations.lock().is_empty(), "{:?}", violations.lock());
        assert!(
            settled_hits.load(Ordering::SeqCst) > 0,
            "a stable source must produce hits"
        );
        assert_directory_holds_exactly_the_live_copies(&rig.cache);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_warms_across_many_paths_exercise_eviction_and_the_sweep() {
        let rig = Rig::with(content_probe());
        let sources: Vec<PathBuf> = (0..12).map(|index| rig.dir.join(format!("envd.{index:02}"))).collect();
        sources
            .iter()
            .enumerate()
            .for_each(|(index, source)| write_generation(source, index as u64));
        let warm_errors = Arc::new(AtomicUsize::new(0));

        let workers: Vec<_> = (0..24usize)
            .map(|worker| {
                let (cache, sources, warm_errors) = (rig.cache.clone(), sources.clone(), warm_errors.clone());
                tokio::spawn(async move {
                    for iteration in 0..40usize {
                        let source = &sources[(worker + iteration) % sources.len()];
                        if cache.lookup(source).await.is_none() && cache.warm(&background(), source).await.is_err() {
                            warm_errors.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.await.unwrap();
        }

        assert_eq!(
            warm_errors.load(Ordering::SeqCst),
            0,
            "no warm should fail while sources and the directory are healthy"
        );
        quiesced(&rig.cache).await;
        assert_directory_holds_exactly_the_live_copies(&rig.cache);
        let state = rig.cache.state.lock();
        assert_eq!(
            state.order.len(),
            state.entries.len(),
            "order and entries must not diverge"
        );
        for entry in state.entries.values() {
            let current = real_stat(&entry.source_path).unwrap();
            assert_eq!(current, entry.identity);
            assert_eq!(payload_trimmed(&entry.local_path), Some(entry.version.clone()));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delivery_is_refused_when_a_promotion_supersedes_the_hit_copy() {
        let rig = Rig::with(content_probe());
        write_generation(&rig.src, 0);
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let resolver = resolver::Resolver::new(rig.cache.clone(), metrics::Op::Live);
        assert_eq!(resolver.version(&background(), &rig.src).await.unwrap(), "v0");
        let first = resolver.entry_for(&rig.src).unwrap();

        write_generation(&rig.src, 1);
        rig.cache.warm(&background(), &rig.src).await.unwrap();

        assert!(
            !first.local_path.exists(),
            "publishing generation 1 must reclaim generation 0's copy"
        );
        assert!(
            resolver.source_path(&rig.src).await.is_none(),
            "a superseded hit must be refused, not served from the changed source"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_deliveries_never_serve_bytes_from_another_version() {
        let rig = Rig::with(content_probe());
        write_generation(&rig.src, 0);
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        let (stop, mutator) = {
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (stopped, cache, source) = (stop.clone(), rig.cache.clone(), rig.src.clone());
            let mutator = tokio::spawn(async move {
                let mut generation = 1;
                while !stopped.load(Ordering::SeqCst) {
                    write_generation(&source, generation);
                    drop(cache.warm(&background(), &source).await);
                    generation += 1;
                    tokio::time::sleep(Duration::from_micros(500)).await;
                }
            });
            (stop, mutator)
        };
        let (from_copy, from_source) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let violations: Violations = Arc::default();

        let callers: Vec<_> = (0..16)
            .map(|_| {
                let (cache, source, violations) = (rig.cache.clone(), rig.src.clone(), violations.clone());
                let (from_copy, from_source) = (from_copy.clone(), from_source.clone());
                tokio::spawn(async move {
                    for _ in 0..60 {
                        let resolver = resolver::Resolver::new(cache.clone(), metrics::Op::Live);
                        let Ok(version) = resolver.version(&background(), &source).await else {
                            continue;
                        };
                        let Some(path) = resolver.source_path(&source).await else {
                            continue;
                        };
                        let Some(payload) = payload_trimmed(&path) else {
                            continue;
                        };
                        if !path.starts_with(cache.dir()) {
                            from_source.fetch_add(1, Ordering::SeqCst);
                            record(
                                &violations,
                                format!("delivered {} from outside the cache directory", path.display()),
                            );
                            continue;
                        }
                        from_copy.fetch_add(1, Ordering::SeqCst);
                        if payload != version {
                            record(
                                &violations,
                                format!(
                                    "delivered copy {} holds {payload:?} but {version:?} was reported",
                                    path.display()
                                ),
                            );
                        }
                    }
                })
            })
            .collect();
        for caller in callers {
            caller.await.unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        mutator.await.unwrap();

        assert!(violations.lock().is_empty(), "{:?}", violations.lock());
        assert!(
            from_copy.load(Ordering::SeqCst) > 0,
            "deliveries must have come from a copy, or nothing was asserted"
        );
        assert_eq!(
            from_source.load(Ordering::SeqCst),
            0,
            "no delivery may come off the mount"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn backoff_suppresses_copies_and_is_not_reported_as_a_failure() {
        let probes = Arc::new(AtomicUsize::new(0));
        let counted = probes.clone();
        let rig = Rig::with_cache(|base| {
            std::fs::write(base.join("blocker"), "x").unwrap();
            let inner = content_probe();
            Cache::builder()
                .dir(base.join("blocker").join("cache"))
                .probe(FnProbe::shared(move |scope, path| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let inner = inner.clone();
                    async move { inner.version(&scope, &path).await }.boxed()
                }))
                .build()
        });
        write_generation(&rig.src, 1);
        let (suppressed, unexpected) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));

        let callers: Vec<_> = (0..24)
            .map(|_| {
                let (cache, source) = (rig.cache.clone(), rig.src.clone());
                let (suppressed, unexpected) = (suppressed.clone(), unexpected.clone());
                tokio::spawn(async move {
                    for _ in 0..10 {
                        match cache.warm(&background(), &source).await {
                            Ok(()) => drop(unexpected.fetch_add(1, Ordering::SeqCst)),
                            Err(error) if error.is(Kind::Suppressed) => drop(suppressed.fetch_add(1, Ordering::SeqCst)),
                            Err(_) => {}
                        }
                    }
                })
            })
            .collect();
        for caller in callers {
            caller.await.unwrap();
        }

        assert_eq!(
            unexpected.load(Ordering::SeqCst),
            0,
            "a broken cache directory reported success"
        );
        assert!(
            suppressed.load(Ordering::SeqCst) > 0,
            "after the first cache-side failure the rest must be suppressed"
        );
        assert_eq!(
            probes.load(Ordering::SeqCst),
            0,
            "a suppressed warm must not reach the probe"
        );

        std::fs::remove_file(rig.dir.join("blocker")).unwrap();
        std::fs::create_dir_all(rig.dir.join("blocker").join("cache")).unwrap();
        let held = rig.cache.warm(&background(), &rig.src).await.unwrap_err();
        assert!(
            held.is(Kind::Suppressed),
            "the backoff must hold even once the directory is usable again"
        );

        rig.cache.note_success(&rig.src);
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        assert!(
            rig.cache.lookup(&rig.src).await.is_some(),
            "the cache must recover once the backoff lapses"
        );
        assert!(probes.load(Ordering::SeqCst) > 0, "the recovered warm must have probed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_warms_under_churn_across_many_paths_and_eviction() {
        let rig = Rig::with(content_probe());
        let sources: Vec<PathBuf> = (0..8).map(|index| rig.dir.join(format!("envd.{index:02}"))).collect();
        sources.iter().for_each(|source| write_generation(source, 1));
        let mutators: Vec<_> = sources
            .iter()
            .map(|source| promoting(source.clone(), 2, Duration::from_millis(1)))
            .collect();
        let (warm_errors, violations): (Arc<AtomicUsize>, Violations) = (Arc::default(), Arc::default());

        let workers: Vec<_> = (0..24usize)
            .map(|worker| {
                let (cache, sources, warm_errors, violations) = (
                    rig.cache.clone(),
                    sources.clone(),
                    warm_errors.clone(),
                    violations.clone(),
                );
                tokio::spawn(async move {
                    for iteration in 0..60usize {
                        let source = &sources[(worker + iteration) % sources.len()];
                        let Some(entry) = cache.lookup(source).await else {
                            if cache.warm(&background(), source).await.is_err() {
                                warm_errors.fetch_add(1, Ordering::SeqCst);
                            }
                            continue;
                        };
                        match payload_trimmed(&entry.local_path) {
                            Some(payload) if payload != entry.version => record(
                                &violations,
                                format!(
                                    "copy {} holds {payload:?} but its entry records {:?}",
                                    entry.local_path.display(),
                                    entry.version
                                ),
                            ),
                            _ => {}
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.await.unwrap();
        }
        let mut promotions = 0;
        for (stop, mutator) in mutators {
            stop.store(true, Ordering::SeqCst);
            promotions += mutator.await.unwrap();
        }

        assert!(violations.lock().is_empty(), "{:?}", violations.lock());
        quiesced(&rig.cache).await;
        assert_eq!(
            warm_errors.load(Ordering::SeqCst),
            0,
            "no warm should fail while sources and the directory are healthy"
        );
        assert!(promotions > 0);
        assert_directory_holds_exactly_the_live_copies(&rig.cache);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_warm_superseded_mid_flight_publishes_nothing_and_releases_its_claim() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let probes = Arc::new(AtomicUsize::new(0));
        let probe = {
            let (entered, release, probes) = (entered.clone(), release.clone(), probes.clone());
            let inner = content_probe();
            FnProbe::shared(move |scope, path| {
                let (entered, release, first, inner) = (
                    entered.clone(),
                    release.clone(),
                    probes.fetch_add(1, Ordering::SeqCst) == 0,
                    inner.clone(),
                );
                async move {
                    if first {
                        entered.notify_one();
                        release.notified().await;
                    }
                    inner.version(&scope, &path).await
                }
                .boxed()
            })
        };
        let rig = Rig::with(probe);
        write_generation(&rig.src, 1);
        let warming = {
            let (cache, source) = (rig.cache.clone(), rig.src.clone());
            tokio::spawn(async move { cache.warm(&background(), &source).await })
        };
        entered.notified().await;

        write_generation(&rig.src, 2);
        release.notify_one();
        warming
            .await
            .unwrap()
            .expect("nothing went wrong, the work was simply overtaken");

        assert!(
            rig.cache.lookup(&rig.src).await.is_none(),
            "a superseded copy must not be published"
        );
        assert!(
            entries_in(&rig.cache_dir())
                .iter()
                .all(|name| name.starts_with(TEMP_PREFIX)),
            "a superseded copy must be reclaimed"
        );
        assert!(rig.cache.state.lock().warming.is_empty());
        rig.cache.warm(&background(), &rig.src).await.unwrap();
        assert_eq!(
            rig.cache.lookup(&rig.src).await.unwrap().version,
            "v2",
            "the next warm must publish the new generation"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_source_stat_in_the_warmer_does_not_block_lookup() {
        let gate = Gate::closed();
        let parked = Arc::new(AtomicUsize::new(0));
        let cache_dir = Arc::new(Mutex::new(PathBuf::new()));
        let rig = Rig::with_cache(|base| {
            *cache_dir.lock() = base.join("cache");
            let (gate, parked, watched) = (gate.clone(), parked.clone(), cache_dir.clone());
            Cache::builder()
                .dir(base.join("cache"))
                .probe(content_probe())
                .stat_source(FnStat::shared(move |path| {
                    let populated = std::fs::read_dir(&*watched.lock()).is_ok_and(|names| names.count() > 0);
                    if populated
                        && parked
                            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        gate.wait();
                    }
                    real_stat(path)
                }))
                .build()
        });
        let _release = OnDrop::run({
            let gate = gate.clone();
            move || gate.open()
        });
        write_generation(&rig.src, 1);
        let warming = {
            let (cache, source) = (rig.cache.clone(), rig.src.clone());
            tokio::spawn(async move { cache.warm(&background(), &source).await })
        };
        assert!(eventually(|| parked.load(Ordering::SeqCst) == 1, Duration::from_secs(10)).await);
        assert!(
            !entries_in(&rig.cache_dir()).is_empty(),
            "the warm must be parked after writing its copy"
        );

        let missed = rig.cache.lookup(&rig.src).await;

        assert!(
            missed.is_none(),
            "nothing is published while the warm is still in flight"
        );
        gate.open();
        warming.await.unwrap().unwrap();
        let entry = rig
            .cache
            .lookup(&rig.src)
            .await
            .expect("the warm must publish once its stalled stat returns");
        assert_eq!(entry.version, "v1");
        assert!(entry.cached());
    }

    #[test]
    fn warm_failures_are_classified_for_the_metric() {
        assert_eq!(warm_result(&error::WarmError::of(Kind::Suppressed)), "suppressed");
    }
}
