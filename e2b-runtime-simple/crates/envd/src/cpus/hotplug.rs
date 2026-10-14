use std::path::PathBuf;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;

use super::sysfs::CpuSysfs;
use super::target_file::{DEFAULT_TARGET_FILE, TargetFile};
use super::{CpuManager, State};
use crate::reaper::Registry;

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    pub max_stalled: usize,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
            max_stalled: 10,
        }
    }
}

impl Backoff {
    pub fn delay(&self, attempt: usize) -> Duration {
        (1..attempt)
            .fold(self.base, |delay, _| if delay < self.cap { delay * 2 } else { delay })
            .min(self.cap)
    }
}

#[derive(Default)]
struct Progress {
    target: usize,
    attempts: usize,
}

#[derive(Default)]
struct Run {
    changed: usize,
    error: Option<String>,
}

struct Step {
    cpu: usize,
    online: bool,
}

pub struct Hotplug<S> {
    sys: S,
    children: Arc<Registry>,
    target_file: TargetFile,
    backoff: Backoff,
    progress: Mutex<Progress>,
    wake: Notify,
    write_started: Mutex<Option<Instant>>,
    mask_warned: Once,
}

#[bon::bon]
impl<S: CpuSysfs> Hotplug<S> {
    #[builder]
    pub fn new(
        sys: S,
        children: Arc<Registry>,
        #[builder(default = PathBuf::from(DEFAULT_TARGET_FILE), into)] target_file: PathBuf,
        #[builder(default)] backoff: Backoff,
    ) -> Arc<Self> {
        Arc::new(Self {
            sys,
            children,
            target_file: TargetFile::new(target_file),
            backoff,
            progress: Mutex::default(),
            wake: Notify::new(),
            write_started: Mutex::new(None),
            mask_warned: Once::new(),
        })
    }
}

impl<S: CpuSysfs> CpuManager for Hotplug<S> {
    fn set_target(&self, count: i64) -> Result<(), String> {
        let target = self.validate_target(count)?;
        let mut progress = self.progress.lock();
        self.target_file.save(target)?;
        *progress = Progress { target, attempts: 0 };
        self.wake.notify_one();
        Ok(())
    }

    fn status(&self) -> State {
        let masks = self
            .sys
            .possible()
            .and_then(|possible| Ok((possible, self.sys.online()?)));
        let (possible, online) = masks.unwrap_or_else(|e| {
            self.mask_warned
                .call_once(|| tracing::warn!(error = %e, "cpus: cannot read CPU masks, reporting zero counts"));
            Default::default()
        });
        let write_pending = self.write_started.lock().map(|started| started.elapsed());
        let progress = self.progress.lock();
        State {
            online: online.len(),
            possible: possible.len(),
            target: progress.target,
            attempts: progress.attempts,
            write_pending: write_pending.unwrap_or_default(),
        }
    }

    fn start(self: Arc<Self>) {
        self.restore_persisted_target();
        tokio::spawn(async move { self.run().await });
    }
}

impl<S: CpuSysfs> Hotplug<S> {
    fn restore_persisted_target(&self) {
        let restored = self
            .target_file
            .load()
            .and_then(|persisted| persisted.map(|count| self.validate_target(count)).transpose());
        match restored {
            Ok(Some(target)) => {
                self.progress.lock().target = target;
                self.wake.notify_one();
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "cpus: ignoring persisted target"),
        }
    }

    async fn run(&self) {
        *self.write_started.lock() = Some(Instant::now());
        self.children.wait_adopted().await;
        *self.write_started.lock() = None;
        loop {
            self.wake.notified().await;
            self.converge().await;
        }
    }

    async fn converge(&self) {
        let mut stalled = 0;
        loop {
            let run = self.reconcile().await;
            let (attempts, target) = self.count_attempt();
            let Some(error) = run.error else {
                return;
            };
            stalled = if run.changed > 0 { 0 } else { stalled + 1 };
            if stalled >= self.backoff.max_stalled {
                tracing::error!(error = %error, attempts, target, "cpus: giving up on target until a new one arrives");
                return;
            }
            let delay = self.backoff.delay(stalled);
            tracing::warn!(error = %error, attempt = attempts, retry_in = ?delay, "cpus: reconcile failed");
            tokio::select! {
                () = self.wake.notified() => stalled = 0,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    fn count_attempt(&self) -> (usize, usize) {
        let mut progress = self.progress.lock();
        progress.attempts += 1;
        (progress.attempts, progress.target)
    }

    async fn reconcile(&self) -> Run {
        let possible = match self.sys.possible() {
            Ok(possible) => possible,
            Err(error) => {
                return Run {
                    changed: 0,
                    error: Some(error),
                };
            }
        };
        let mut run = Run::default();
        loop {
            match self.settle_one(&possible, run.changed).await {
                Ok(true) => run.changed += 1,
                Ok(false) => return run,
                Err(error) => {
                    run.error = Some(error);
                    return run;
                }
            }
        }
    }

    async fn settle_one(&self, possible: &[usize], changed: usize) -> Result<bool, String> {
        let target = self.progress.lock().target;
        let online = self.sys.online()?;
        let Some(step) = next_step(target, possible, &online)? else {
            if changed > 0 {
                tracing::info!(
                    online = online.len(),
                    possible = possible.len(),
                    changed,
                    "cpus: online set matches target"
                );
            }
            return Ok(false);
        };
        self.write(&step).await?;
        if self.sys.online()?.contains(&step.cpu) != step.online {
            return Err(format!(
                "cpu{}: write reported success but the online mask did not change",
                step.cpu
            ));
        }
        Ok(true)
    }

    async fn write(&self, step: &Step) -> Result<(), String> {
        *self.write_started.lock() = Some(Instant::now());
        let written = self.sys.set_online(step.cpu, step.online).await;
        *self.write_started.lock() = None;
        written.map_err(|e| format!("set cpu{} online={}: {e}", step.cpu, step.online))?;
        tracing::debug!(cpu = step.cpu, online = step.online, "cpus: changed CPU state");
        Ok(())
    }

    fn validate_target(&self, count: i64) -> Result<usize, String> {
        let possible = self.sys.possible()?.len();
        usize::try_from(count)
            .ok()
            .filter(|count| (1..=possible).contains(count))
            .ok_or_else(|| format!("cpu target {count} outside 1..{possible}"))
    }
}

fn next_step(target: usize, possible: &[usize], online: &[usize]) -> Result<Option<Step>, String> {
    match online.len().cmp(&target) {
        std::cmp::Ordering::Equal => Ok(None),
        std::cmp::Ordering::Less => lowest_offline(possible, online)
            .map(|cpu| Some(Step { cpu, online: true }))
            .ok_or_else(|| format!("cpu target {target} exceeds {} possible CPUs", possible.len())),
        std::cmp::Ordering::Greater => match online.last() {
            Some(&cpu) if cpu != 0 => Ok(Some(Step { cpu, online: false })),
            _ => Err(format!("cpu target {target} leaves only CPU0, which cannot go offline")),
        },
    }
}

fn lowest_offline(possible: &[usize], online: &[usize]) -> Option<usize> {
    possible.iter().copied().find(|cpu| !online.contains(cpu))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Write;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use parking_lot::Condvar;

    use super::*;

    type Hook = Arc<dyn Fn(usize, bool) -> Result<(), String> + Send + Sync>;

    struct FakeState {
        online: Vec<bool>,
        error: Option<String>,
        inert: HashSet<usize>,
    }

    struct FakeSysfs {
        state: Mutex<FakeState>,
        hook: Mutex<Option<Hook>>,
    }

    impl FakeSysfs {
        fn new(possible: usize, online: usize) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeState {
                    online: (0..possible).map(|cpu| cpu < online).collect(),
                    error: None,
                    inert: HashSet::new(),
                }),
                hook: Mutex::new(None),
            })
        }

        fn failing(message: &str) -> Arc<Self> {
            let sys = Self::new(0, 0);
            sys.state.lock().error = Some(message.to_owned());
            sys
        }

        fn set_hook(&self, hook: impl Fn(usize, bool) -> Result<(), String> + Send + Sync + 'static) {
            *self.hook.lock() = Some(Arc::new(hook));
        }

        fn clear_hook(&self) {
            *self.hook.lock() = None;
        }

        fn online_cpus(&self) -> Vec<usize> {
            self.state
                .lock()
                .online
                .iter()
                .enumerate()
                .filter_map(|(cpu, online)| online.then_some(cpu))
                .collect()
        }

        fn set_state(&self, cpu: usize, online: bool) {
            self.state.lock().online[cpu] = online;
        }

        fn make_inert(&self, cpu: usize) {
            self.state.lock().inert.insert(cpu);
        }
    }

    impl CpuSysfs for Arc<FakeSysfs> {
        fn possible(&self) -> Result<Vec<usize>, String> {
            let state = self.state.lock();
            state
                .error
                .clone()
                .map_or_else(|| Ok((0..state.online.len()).collect()), Err)
        }

        fn online(&self) -> Result<Vec<usize>, String> {
            let error = self.state.lock().error.clone();
            error.map_or_else(|| Ok(self.online_cpus()), Err)
        }

        async fn set_online(&self, cpu: usize, online: bool) -> Result<(), String> {
            let hook = self.hook.lock().clone();
            hook.map_or(Ok(()), |hook| hook(cpu, online))?;
            let mut state = self.state.lock();
            if cpu == 0 || cpu >= state.online.len() {
                return Err(format!("cpu{cpu} has no online file"));
            }
            if !state.inert.contains(&cpu) {
                state.online[cpu] = online;
            }
            Ok(())
        }
    }

    struct Gate(Arc<(Mutex<bool>, Condvar)>);

    impl Gate {
        fn closed() -> Self {
            Self(Arc::new((Mutex::new(false), Condvar::new())))
        }

        fn wait(&self) {
            let (open, opened) = &*self.0;
            let mut open = open.lock();
            while !*open {
                opened.wait(&mut open);
            }
        }

        fn open(&self) {
            *self.0.0.lock() = true;
            self.0.1.notify_all();
        }

        fn handle(&self) -> Gate {
            Gate(self.0.clone())
        }
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            self.open();
        }
    }

    struct Fixture {
        manager: Arc<Hotplug<Arc<FakeSysfs>>>,
        sys: Arc<FakeSysfs>,
        children: Arc<Registry>,
        directory: Arc<tempfile::TempDir>,
    }

    impl Fixture {
        fn new(sys: Arc<FakeSysfs>) -> Self {
            Self::with_directory(sys, Arc::new(tempfile::tempdir().unwrap()))
        }

        fn with_directory(sys: Arc<FakeSysfs>, directory: Arc<tempfile::TempDir>) -> Self {
            let children = Arc::new(Registry::default());
            let manager = Hotplug::builder()
                .sys(sys.clone())
                .children(children.clone())
                .target_file(directory.path().join("cpu-target"))
                .backoff(Backoff {
                    base: Duration::from_millis(10),
                    cap: Duration::from_millis(40),
                    max_stalled: 4,
                })
                .build();
            Self {
                manager,
                sys,
                children,
                directory,
            }
        }

        fn target_file(&self) -> PathBuf {
            self.directory.path().join("cpu-target")
        }

        fn restarted(&self, sys: Arc<FakeSysfs>) -> Fixture {
            Fixture::with_directory(sys, self.directory.clone())
        }

        fn start(&self) {
            self.manager.clone().start();
        }

        fn status(&self) -> State {
            self.manager.status()
        }

        async fn eventually_online(&self, count: usize) {
            eventually(|| self.sys.online_cpus().len() == count).await;
        }
    }

    fn converged(state: State) -> bool {
        state.target == 0 || state.online == state.target
    }

    async fn eventually(condition: impl Fn() -> bool) {
        for _ in 0..1000 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never held");
    }

    fn fail_first(count: usize) -> impl Fn(usize, bool) -> Result<(), String> + Send + Sync {
        let calls = AtomicUsize::new(0);
        move |_, _| match calls.fetch_add(1, Ordering::SeqCst) < count {
            true => Err("injected EBUSY".to_owned()),
            false => Ok(()),
        }
    }

    fn fail_always(_: usize, _: bool) -> Result<(), String> {
        Err("injected EIO".to_owned())
    }

    fn adopt_waiting_child(children: &Registry) -> std::io::PipeWriter {
        let (reader, writer) = std::io::pipe().unwrap();
        let child = Command::new("cat").stdin(reader).spawn().unwrap();
        children.adopt(child.id() as i32);
        std::mem::forget(child);
        writer
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_brings_the_online_set_to_the_target() {
        for (name, possible, online, target, wanted) in [
            (
                "grow onlines the lowest offline CPUs",
                16,
                vec![0, 1, 2, 3],
                8,
                (0..8).collect::<Vec<_>>(),
            ),
            (
                "grow fills gaps before extending",
                8,
                vec![0, 1, 3, 5],
                6,
                (0..6).collect(),
            ),
            (
                "shrink offlines the highest online CPUs",
                16,
                (0..8).collect(),
                4,
                (0..4).collect(),
            ),
            (
                "shrink takes the highest across gaps",
                8,
                vec![0, 1, 3, 5],
                2,
                vec![0, 1],
            ),
            (
                "matching target writes nothing",
                16,
                vec![0, 1, 2, 3],
                4,
                vec![0, 1, 2, 3],
            ),
            ("all possible CPUs", 8, vec![0, 1], 8, (0..8).collect()),
            ("target 1 keeps CPU0", 8, vec![0, 1, 2, 3], 1, vec![0]),
        ] {
            let sys = FakeSysfs::new(possible, 0);
            online.iter().for_each(|cpu| sys.set_state(*cpu, true));
            let fixture = Fixture::new(sys);
            fixture.manager.set_target(target).unwrap();

            let run = tokio::time::timeout(Duration::from_secs(5), fixture.manager.reconcile())
                .await
                .unwrap();

            assert_eq!(run.error, None, "{name}");
            assert_eq!(fixture.sys.online_cpus(), wanted, "{name}");
            let expected = State {
                online: wanted.len(),
                possible,
                target: target as usize,
                ..State::default()
            };
            assert_eq!(fixture.status(), expected, "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_without_effect_is_an_error() {
        let sys = FakeSysfs::new(8, 4);
        sys.make_inert(5);
        let fixture = Fixture::new(sys);
        fixture.manager.set_target(8).unwrap();

        let run = tokio::time::timeout(Duration::from_secs(5), fixture.manager.reconcile())
            .await
            .unwrap();

        assert!(run.error.unwrap().contains("cpu5: write reported success"));
        assert_eq!(run.changed, 1, "cpu4 came up before the inert one");
        assert_eq!(fixture.sys.online_cpus(), vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_target_rejects_out_of_range_counts() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));

        for count in [0, -1, 17] {
            assert!(fixture.manager.set_target(count).is_err(), "{count}");
        }

        assert!(
            !fixture.target_file().exists(),
            "a rejected target must not be persisted"
        );
        assert_eq!(fixture.sys.online_cpus(), vec![0, 1, 2, 3]);
        assert!(converged(fixture.status()), "no target means nothing to converge on");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_target_names_the_range_it_refused() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));

        assert_eq!(
            fixture.manager.set_target(32),
            Err("cpu target 32 outside 1..16".to_owned())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_applies_the_target_in_the_background() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));
        fixture.start();

        fixture.manager.set_target(12).unwrap();

        fixture.eventually_online(12).await;
        eventually(|| converged(fixture.status()) && fixture.status().attempts == 1).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_target_is_read_again_before_each_cpu() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));
        let writes = Arc::new(AtomicUsize::new(0));
        let manager = fixture.manager.clone();
        let counted = writes.clone();
        fixture.sys.set_hook(move |cpu, online| {
            counted.fetch_add(1, Ordering::SeqCst);
            match cpu == 6 && online {
                true => manager.set_target(6),
                false => Ok(()),
            }
        });
        fixture.start();

        fixture.manager.set_target(12).unwrap();

        fixture.eventually_online(6).await;
        eventually(|| converged(fixture.status()) && fixture.status().attempts > 0).await;
        assert_eq!(writes.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_runs_are_retried_until_converged() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));
        fixture.sys.set_hook(fail_first(2));
        fixture.start();

        fixture.manager.set_target(8).unwrap();

        fixture.eventually_online(8).await;
        eventually(|| fixture.status().attempts == 3).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn it_gives_up_when_stalled_until_a_new_target_arrives() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));
        fixture.sys.set_hook(fail_always);
        fixture.start();

        fixture.manager.set_target(8).unwrap();

        eventually(|| fixture.status().attempts == 4).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fixture.status().attempts,
            4,
            "no retries after max_stalled runs without progress"
        );
        assert_eq!(fixture.sys.online_cpus(), vec![0, 1, 2, 3]);

        fixture.sys.clear_hook();
        fixture.manager.set_target(6).unwrap();

        fixture.eventually_online(6).await;
        eventually(|| fixture.status().attempts == 1).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn progress_resets_the_stall_budget() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));
        let calls = AtomicUsize::new(0);
        fixture
            .sys
            .set_hook(move |_, _| match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(()),
                _ => Err("injected EIO".to_owned()),
            });
        fixture.start();

        fixture.manager.set_target(8).unwrap();

        eventually(|| fixture.status().attempts == 5).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fixture.status().attempts,
            5,
            "no retries after max_stalled runs without progress"
        );
        assert_eq!(fixture.sys.online_cpus().len(), 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_target_during_backoff_gets_a_fresh_budget() {
        let sys = FakeSysfs::new(16, 4);
        sys.set_hook(fail_always);
        let directory = tempfile::tempdir().unwrap();
        let children = Arc::new(Registry::default());
        let manager = Hotplug::builder()
            .sys(sys.clone())
            .children(children)
            .target_file(directory.path().join("cpu-target"))
            .backoff(Backoff {
                base: Duration::from_millis(300),
                cap: Duration::from_millis(300),
                max_stalled: 4,
            })
            .build();
        manager.clone().start();

        manager.set_target(8).unwrap();
        eventually(|| manager.status().attempts == 3).await;

        manager.set_target(6).unwrap();

        eventually(|| manager.status().attempts == 4).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn slow_progress_is_not_given_up() {
        let fixture = Fixture::new(FakeSysfs::new(16, 4));
        let calls = AtomicUsize::new(0);
        fixture
            .sys
            .set_hook(move |_, _| match calls.fetch_add(1, Ordering::SeqCst) % 2 {
                0 => Err("injected EBUSY".to_owned()),
                _ => Ok(()),
            });
        fixture.start();

        fixture.manager.set_target(12).unwrap();

        fixture.eventually_online(12).await;
        eventually(|| fixture.status().attempts == 9).await;
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let backoff = Backoff {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(5),
            max_stalled: 10,
        };

        for (attempt, expected) in [(1, 1), (2, 2), (3, 4), (4, 5), (9, 5)] {
            assert_eq!(backoff.delay(attempt), Duration::from_secs(expected), "{attempt}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_resumes_a_persisted_target() {
        let sys = FakeSysfs::new(16, 4);
        let first = Fixture::new(sys.clone());
        first.manager.set_target(12).unwrap();

        let restarted = first.restarted(sys.clone());
        restarted.start();

        restarted.eventually_online(12).await;
        assert_eq!(restarted.status().target, 12);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_ignores_an_invalid_persisted_target() {
        for (name, content) in [
            ("not a number", "lots\n".to_owned()),
            ("zero", "0".to_owned()),
            ("negative", "-3".to_owned()),
            ("above possible", "17".to_owned()),
            ("empty", String::new()),
            ("longer than bound", "1".repeat(1 << 20)),
        ] {
            let sys = FakeSysfs::new(16, 4);
            let fixture = Fixture::new(sys.clone());
            std::fs::write(fixture.target_file(), content).unwrap();

            fixture.start();

            assert_eq!(fixture.status().target, 0, "{name}");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(sys.online_cpus().len(), 4, "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stuck_write_is_reported_pending_and_blocks_nothing_else() {
        let sys = FakeSysfs::new(8, 4);
        let gate = Gate::closed();
        let blocker = gate.handle();
        sys.set_hook(move |cpu, _| {
            if cpu == 5 {
                blocker.wait();
            }
            Ok(())
        });
        let fixture = Fixture::new(sys.clone());
        fixture.start();

        fixture.manager.set_target(8).unwrap();

        eventually(|| fixture.status().write_pending > Duration::from_millis(50)).await;
        assert_eq!(
            sys.online_cpus(),
            vec![0, 1, 2, 3, 4],
            "CPUs before the stuck one came up"
        );

        gate.open();

        fixture.eventually_online(8).await;
        eventually(|| fixture.status().write_pending == Duration::ZERO).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_target_changed_during_a_stuck_write_wins() {
        let sys = FakeSysfs::new(4, 2);
        let gate = Gate::closed();
        let blocker = gate.handle();
        let offlined = Arc::new(AtomicBool::new(false));
        let seen = offlined.clone();
        let stuck = Arc::new(AtomicBool::new(false));
        let entered = stuck.clone();
        sys.set_hook(move |cpu, online| {
            match (cpu, online) {
                (2, true) => {
                    entered.store(true, Ordering::SeqCst);
                    blocker.wait()
                }
                (2, false) => seen.store(true, Ordering::SeqCst),
                _ => {}
            }
            Ok(())
        });
        let fixture = Fixture::new(sys.clone());
        fixture.start();

        fixture.manager.set_target(3).unwrap();
        eventually(|| stuck.load(Ordering::SeqCst)).await;
        fixture.manager.set_target(2).unwrap();

        gate.open();

        eventually(|| offlined.load(Ordering::SeqCst)).await;
        eventually(|| converged(fixture.status())).await;
        assert_eq!(sys.online_cpus(), vec![0, 1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_inherited_write_lands_before_reconciling() {
        let sys = FakeSysfs::new(4, 2);
        let writes = Arc::new(AtomicUsize::new(0));
        let counted = writes.clone();
        sys.set_hook(move |_, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let fixture = Fixture::new(sys.clone());
        let inherited = adopt_waiting_child(&fixture.children);
        fixture.manager.set_target(2).unwrap();

        fixture.start();

        eventually(|| fixture.status().write_pending > Duration::ZERO).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            writes.load(Ordering::SeqCst),
            0,
            "nothing is written while the inherited write runs"
        );

        sys.set_state(2, true);
        drop(inherited);

        eventually(|| sys.online_cpus().len() == 2 && converged(fixture.status())).await;
        assert_eq!(sys.online_cpus(), vec![0, 1]);
        assert_eq!(fixture.status().write_pending, Duration::ZERO);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_inherited_write_stops_being_pending_once_reaped() {
        let fixture = Fixture::new(FakeSysfs::new(4, 2));
        let inherited = adopt_waiting_child(&fixture.children);
        fixture.start();

        eventually(|| fixture.status().write_pending > Duration::ZERO).await;
        drop(inherited);

        eventually(|| fixture.status().write_pending == Duration::ZERO).await;
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_warns_once_without_cpu_masks() {
        let fixture = Fixture::new(FakeSysfs::failing("no sysfs"));
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt().with_writer(captured.clone()).finish();

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..3 {
                assert_eq!(fixture.status(), State::default());
            }
        });

        let logs = String::from_utf8(captured.0.lock().clone()).unwrap();
        assert_eq!(logs.matches("cannot read CPU masks").count(), 1);
    }
}
