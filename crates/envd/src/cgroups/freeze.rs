use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::hierarchy::{
    AuditResult, PROC_SELF_CGROUP, allowlisted, audit_frozen_state, descend_set, join, sample_path, scan_guest_frozen,
    self_cgroup_path,
};
use super::spawn::SpawnBarrier;
use super::{
    CgroupError, CgroupManager, Failure, ProcessType, children_of, freeze_at, freeze_requested_at, frozen_at,
    process_type_name, unfreeze_at,
};
use crate::util::fs::clean;

pub const THAW_WATCHDOG_WINDOW: Duration = Duration::from_secs(10 * 60);
pub const THAW_MAX_CGROUPS: usize = 8192;
const FREEZE_MAX_CGROUPS: usize = 512;
const FREEZE_POLL_INTERVAL: Duration = Duration::from_millis(2);
pub const WORKLOAD_PROCESS_TYPES: [ProcessType; 2] = [ProcessType::User, ProcessType::Pty];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreezeMode {
    #[default]
    Legacy,
    Hierarchy,
}

impl FreezeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FreezeMode::Legacy => "legacy",
            FreezeMode::Hierarchy => "hierarchy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreezeOptions {
    pub max_wait: Duration,
    pub mode: FreezeMode,
    pub max_cgroups: i64,
}

impl FreezeOptions {
    fn max_cgroups(&self) -> usize {
        match usize::try_from(self.max_cgroups) {
            Ok(0) | Err(_) => FREEZE_MAX_CGROUPS,
            Ok(max) => max.min(THAW_MAX_CGROUPS),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreezeResult {
    pub requested: i64,
    pub frozen: i64,
    pub not_frozen: i64,
    pub scan_failed: i64,
    pub pre_frozen: i64,
    pub failed: i64,
    pub vanished: i64,
    pub vanished_paths: Vec<String>,
    pub unobservable: i64,
    pub sweep: Duration,
    pub mode: FreezeMode,
    pub visited: i64,
    pub allowlisted: i64,
    pub truncated: bool,
    pub wait: Duration,
}

impl FreezeResult {
    pub fn all_frozen(&self) -> bool {
        self.not_frozen == 0 && self.failed == 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThawResult {
    pub visited: usize,
    pub thawed: usize,
    pub failed: usize,
    pub truncated: bool,
    pub preserved: usize,
    pub discovered: bool,
}

#[derive(Default)]
struct Sweep {
    last_mode: Option<FreezeMode>,
    generation: u64,
    guest_frozen: HashSet<PathBuf>,
    freeze_active: bool,
}

#[derive(Default)]
struct Watchdog {
    generation: u64,
    cancel: Option<CancellationToken>,
}

enum Target {
    Static(ProcessType),
    Walked(PathBuf),
}

#[derive(Default)]
struct Poll {
    frozen: i64,
    failed: i64,
    unobservable: i64,
    vanished: i64,
    vanished_paths: Vec<String>,
    errors: Vec<String>,
}

pub struct WorkloadFreezer {
    manager: Arc<CgroupManager>,
    lock: Arc<Semaphore>,
    pub(super) spawns: SpawnBarrier,
    thawed: watch::Sender<u64>,
    sweep: Mutex<Sweep>,
    watchdog: Mutex<Watchdog>,
    watchdog_window: Duration,
    proc_self_cgroup: PathBuf,
}

impl Target {
    fn label(&self) -> String {
        match self {
            Target::Static(process_type) => process_type_name(*process_type).to_owned(),
            Target::Walked(path) => path.display().to_string(),
        }
    }

    fn frozen(&self, manager: &CgroupManager) -> Result<bool, CgroupError> {
        match self {
            Target::Static(process_type) => manager.frozen(*process_type),
            Target::Walked(path) => frozen_at(path),
        }
    }

    fn tolerates_vanish(&self) -> bool {
        matches!(self, Target::Walked(_))
    }
}

impl WorkloadFreezer {
    pub fn new(manager: Arc<CgroupManager>) -> Arc<Self> {
        Self::with(
            manager,
            THAW_WATCHDOG_WINDOW,
            PathBuf::from(PROC_SELF_CGROUP),
            SpawnBarrier::default(),
        )
    }

    fn with(
        manager: Arc<CgroupManager>,
        watchdog_window: Duration,
        proc_self_cgroup: PathBuf,
        spawns: SpawnBarrier,
    ) -> Arc<Self> {
        Arc::new(Self {
            manager,
            lock: Arc::new(Semaphore::new(1)),
            spawns,
            thawed: watch::Sender::new(0),
            sweep: Mutex::default(),
            watchdog: Mutex::default(),
            watchdog_window,
            proc_self_cgroup,
        })
    }

    pub fn manager(&self) -> &CgroupManager {
        &self.manager
    }

    pub async fn freeze_hold(
        self: &Arc<Self>,
        options: FreezeOptions,
    ) -> (OwnedSemaphorePermit, FreezeResult, Vec<String>) {
        let permit = self
            .lock
            .clone()
            .acquire_owned()
            .await
            .expect("the freezer lock is never closed");
        let (result, errors) = self.freeze_locked(options).await;
        (permit, result, errors)
    }

    pub fn thawed(&self) -> watch::Receiver<u64> {
        self.thawed.subscribe()
    }

    pub fn guest_frozen_paths(&self) -> Vec<String> {
        let Some(root) = self.manager.walk_root() else {
            return Vec::new();
        };
        let root = clean(root);
        let mut relative: Vec<String> = self
            .sweep
            .lock()
            .guest_frozen
            .iter()
            .filter_map(|path| {
                clean(path)
                    .strip_prefix(&root)
                    .ok()
                    .map(|rel| rel.display().to_string())
            })
            .map(|rel| if rel.is_empty() { ".".to_owned() } else { rel })
            .collect();
        relative.sort();
        relative
    }

    pub fn set_guest_frozen_paths(&self, relative: &[String]) {
        let Some(root) = self.manager.walk_root() else {
            return;
        };
        if relative.is_empty() {
            return;
        }
        self.sweep.lock().guest_frozen = relative.iter().map(|rel| join(root, rel)).collect();
    }

    pub async fn resume_frozen(self: &Arc<Self>) -> Result<(), String> {
        self.sweep.lock().freeze_active = true;
        self.arm_watchdog();
        self.spawns
            .raise()
            .await
            .map_err(|e| format!("raise the spawn barrier for an inherited freeze: {e}"))
    }

    pub async fn freeze(self: &Arc<Self>, options: FreezeOptions) -> (FreezeResult, Vec<String>) {
        let _permit = self.lock.acquire().await.expect("the freezer lock is never closed");
        self.freeze_locked(options).await
    }

    pub async fn unfreeze(self: &Arc<Self>) -> Result<ThawResult, String> {
        let _permit = self.lock.acquire().await.expect("the freezer lock is never closed");
        let (result, errors) = self.unfreeze_locked(THAW_MAX_CGROUPS);
        match errors.is_empty() {
            true => Ok(result),
            false => Err(errors.join("\n")),
        }
    }

    pub fn audit(&self) -> Option<Result<AuditResult, String>> {
        let root = self.manager.walk_root()?;
        let (mode, generation) = self.sweep_state();
        let audited = audit_frozen_state(root, &self.proc_self_cgroup, THAW_MAX_CGROUPS, mode);
        Some(audited.map(|audit| match self.sweep_state().1 == generation {
            true => audit,
            false => AuditResult::default(),
        }))
    }

    async fn freeze_locked(self: &Arc<Self>, options: FreezeOptions) -> (FreezeResult, Vec<String>) {
        if let Err(refused) = self.spawns.raise().await {
            let refusal = FreezeResult {
                mode: options.mode,
                failed: WORKLOAD_PROCESS_TYPES.len() as i64,
                ..FreezeResult::default()
            };
            return (refusal, vec![refused.to_string()]);
        }
        let mut result = FreezeResult::default();
        let started = Instant::now();
        let root = self.manager.walk_root().map(Path::to_path_buf);
        let rescan = root.is_some() && !self.freeze_in_effect();
        if let (true, Some(root)) = (rescan, &root) {
            let scan = scan_guest_frozen(root, &self.proc_self_cgroup, THAW_MAX_CGROUPS);
            result.scan_failed = scan.errors.len() as i64;
            result.truncated |= scan.truncated;
            self.sweep.lock().guest_frozen = scan.frozen;
        }
        result.pre_frozen = self.sweep.lock().guest_frozen.len() as i64;
        let (pending, mut errors) = match (options.mode, &root) {
            (FreezeMode::Hierarchy, Some(root)) => {
                result.mode = FreezeMode::Hierarchy;
                self.sweep_hierarchy(root, &mut result, options.max_cgroups(), rescan)
            }
            _ => {
                result.mode = FreezeMode::Legacy;
                self.sweep_legacy(&mut result)
            }
        };
        result.sweep = started.elapsed();
        let poll = self.await_frozen(&pending, options.max_wait).await;
        result.frozen = poll.frozen;
        result.unobservable = poll.unobservable;
        result.vanished += poll.vanished;
        poll.vanished_paths
            .iter()
            .for_each(|path| sample_path(&mut result.vanished_paths, Path::new(path)));
        result.not_frozen = result.requested - poll.frozen - poll.failed - poll.unobservable - poll.vanished;
        result.failed += poll.failed;
        result.wait = started.elapsed().saturating_sub(result.sweep);
        errors.extend(poll.errors);
        let window_open = {
            let mut sweep = self.sweep.lock();
            sweep.last_mode = Some(result.mode);
            sweep.generation += 1;
            sweep.freeze_active |= result.requested > 0;
            sweep.freeze_active
        };
        if !window_open {
            self.spawns.lower();
        }
        if result.requested > 0 {
            self.arm_watchdog();
        }
        (result, errors)
    }

    fn sweep_legacy(&self, result: &mut FreezeResult) -> (Vec<Target>, Vec<String>) {
        let mut errors = Vec::new();
        let mut pending = Vec::new();
        for process_type in WORKLOAD_PROCESS_TYPES {
            match self.manager.freeze(process_type) {
                Ok(()) => {
                    result.requested += 1;
                    pending.push(Target::Static(process_type));
                }
                Err(e) => {
                    errors.push(format!("freeze {} cgroup: {e}", process_type_name(process_type)));
                    result.failed += 1;
                }
            }
        }
        (pending, errors)
    }

    fn sweep_hierarchy(
        &self,
        root: &Path,
        result: &mut FreezeResult,
        max_cgroups: usize,
        adopt_late_freezes: bool,
    ) -> (Vec<Target>, Vec<String>) {
        let own = match self_cgroup_path(&self.proc_self_cgroup, root) {
            Ok(own) => own,
            Err(e) => {
                result.mode = FreezeMode::Legacy;
                let (pending, errors) = self.sweep_legacy(result);
                let fallback = format!("locate envd cgroup, falling back to legacy mode: {e}");
                return (pending, std::iter::once(fallback).chain(errors).collect());
            }
        };
        let guest_frozen = self.sweep.lock().guest_frozen.clone();
        let (mut pending, mut errors) = self.sweep_legacy(result);
        let descend = descend_set(root, &own);
        let inside: HashSet<PathBuf> = descend
            .iter()
            .cloned()
            .chain(
                WORKLOAD_PROCESS_TYPES
                    .iter()
                    .filter_map(|process_type| self.manager.path_of(*process_type).map(Path::to_path_buf)),
            )
            .collect();
        let mut late_guest = Vec::new();
        'walk: for ancestor in &descend {
            let children = match children_of(ancestor) {
                Ok(children) => children,
                Err(e) if e.vanished() => continue,
                Err(e) => {
                    errors.push(format!("list children of {}: {e}", ancestor.display()));
                    result.failed += 1;
                    continue;
                }
            };
            for child in children {
                if result.visited >= max_cgroups as i64 {
                    result.truncated = true;
                    break 'walk;
                }
                result.visited += 1;
                if inside.contains(&child) {
                    continue;
                }
                if allowlisted(root, &child) {
                    result.allowlisted += 1;
                    continue;
                }
                if guest_frozen.contains(&child) {
                    continue;
                }
                if adopt_late_freezes {
                    match freeze_requested_at(&child) {
                        Err(e) if e.vanished() => {
                            record_vanished(result, &child);
                            continue;
                        }
                        Ok(true) => {
                            late_guest.push(child);
                            continue;
                        }
                        _ => {}
                    }
                }
                match freeze_at(&child) {
                    Err(e) if e.vanished() => record_vanished(result, &child),
                    Err(e) => {
                        errors.push(format!("freeze {}: {e}", child.display()));
                        result.failed += 1;
                    }
                    Ok(()) => {
                        result.requested += 1;
                        pending.push(Target::Walked(child));
                    }
                }
            }
        }
        self.adopt_late_guest_freezes(result, late_guest);
        (pending, errors)
    }

    fn adopt_late_guest_freezes(&self, result: &mut FreezeResult, paths: Vec<PathBuf>) {
        let mut sweep = self.sweep.lock();
        paths
            .into_iter()
            .filter(|path| sweep.guest_frozen.insert(path.clone()))
            .for_each(|_| result.pre_frozen += 1);
    }

    async fn await_frozen(&self, targets: &[Target], budget: Duration) -> Poll {
        let mut poll = Poll::default();
        if targets.is_empty() || budget.is_zero() {
            return poll;
        }
        let deadline = Instant::now() + budget;
        let mut remaining: Vec<&Target> = targets.iter().collect();
        loop {
            remaining.retain(|target| match target.frozen(&self.manager) {
                Err(e) if e.failure == Failure::Unobservable => {
                    poll.unobservable += 1;
                    false
                }
                Err(e) if e.vanished() && target.tolerates_vanish() => {
                    poll.vanished += 1;
                    sample_path(&mut poll.vanished_paths, Path::new(&target.label()));
                    false
                }
                Err(e) => {
                    poll.failed += 1;
                    poll.errors.push(format!("read {} cgroup.events: {e}", target.label()));
                    false
                }
                Ok(true) => {
                    poll.frozen += 1;
                    false
                }
                Ok(false) => true,
            });
            if remaining.is_empty() || Instant::now() >= deadline {
                return poll;
            }
            tokio::time::sleep(FREEZE_POLL_INTERVAL.min(deadline - Instant::now())).await;
        }
    }

    fn unfreeze_locked(self: &Arc<Self>, max_cgroups: usize) -> (ThawResult, Vec<String>) {
        {
            let mut sweep = self.sweep.lock();
            sweep.last_mode = None;
            sweep.generation += 1;
        }
        let mut result = ThawResult::default();
        let root = self.manager.walk_root().map(Path::to_path_buf);
        let discover = root.is_some() && self.freeze_in_effect();
        let static_failures: Vec<(ProcessType, CgroupError)> = WORKLOAD_PROCESS_TYPES
            .iter()
            .filter_map(|process_type| self.manager.unfreeze(*process_type).err().map(|e| (*process_type, e)))
            .collect();
        let mut errors: Vec<String> = static_failures
            .iter()
            .map(|(process_type, e)| format!("unfreeze {} cgroup: {e}", process_type_name(*process_type)))
            .collect();
        result.failed += errors.len();
        if static_failures.iter().all(|(_, e)| e.vanished()) {
            self.spawns.lower();
        }
        if let (true, Some(root)) = (discover, &root) {
            result.discovered = true;
            errors.extend(self.thaw_discovered(root, &mut result, max_cgroups));
        }
        if errors.is_empty() && !result.truncated && result.failed == 0 {
            self.disarm_watchdog();
            self.sweep.lock().freeze_active = false;
        }
        self.thawed.send_modify(|generation| *generation += 1);
        (result, errors)
    }

    fn thaw_discovered(&self, root: &Path, result: &mut ThawResult, max_cgroups: usize) -> Vec<String> {
        let own = match self_cgroup_path(&self.proc_self_cgroup, root) {
            Ok(own) => own,
            Err(e) => return vec![format!("locate envd cgroup for the thaw: {e}")],
        };
        let guest_frozen = self.sweep.lock().guest_frozen.clone();
        let on_chain: HashSet<PathBuf> = super::hierarchy::ancestor_chain(root, &own).into_iter().collect();
        let mut errors = Vec::new();
        let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
        while let Some(current) = queue.pop_front() {
            if result.visited >= max_cgroups {
                result.truncated = true;
                errors.push(format!(
                    "thaw walk truncated at {max_cgroups} cgroups: some may still be frozen"
                ));
                return errors;
            }
            result.visited += 1;
            let mut skip = on_chain.contains(&current);
            if !skip && guest_frozen.contains(&current) {
                result.preserved += 1;
                skip = true;
            }
            if !skip {
                self.thaw_one(&current, result, &mut errors);
            }
            match children_of(&current) {
                Ok(children) => queue.extend(children),
                Err(e) if e.vanished() => {}
                Err(e) => {
                    errors.push(format!("list children of {}: {e}", current.display()));
                    result.failed += 1;
                }
            }
        }
        errors
    }

    fn thaw_one(&self, path: &Path, result: &mut ThawResult, errors: &mut Vec<String>) {
        match freeze_requested_at(path) {
            Err(e) if e.vanished() => {}
            Err(e) => {
                errors.push(format!("read freeze state of {}: {e}", path.display()));
                result.failed += 1;
            }
            Ok(false) => {}
            Ok(true) => match unfreeze_at(path) {
                Err(e) if e.vanished() => {}
                Err(e) => {
                    errors.push(format!("thaw {}: {e}", path.display()));
                    result.failed += 1;
                }
                Ok(()) => result.thawed += 1,
            },
        }
    }

    fn freeze_in_effect(&self) -> bool {
        self.sweep.lock().freeze_active
            || WORKLOAD_PROCESS_TYPES
                .iter()
                .any(|process_type| self.manager.frozen(*process_type).is_ok_and(|frozen| frozen))
    }

    fn sweep_state(&self) -> (Option<FreezeMode>, u64) {
        let sweep = self.sweep.lock();
        (sweep.last_mode, sweep.generation)
    }

    fn arm_watchdog(self: &Arc<Self>) {
        if self.watchdog_window.is_zero() {
            return;
        }
        let cancel = CancellationToken::new();
        let generation = {
            let mut watchdog = self.watchdog.lock();
            if let Some(previous) = watchdog.cancel.replace(cancel.clone()) {
                previous.cancel();
            }
            watchdog.generation += 1;
            watchdog.generation
        };
        let freezer = self.clone();
        let window = self.watchdog_window;
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(window) => freezer.thaw_for_watchdog(generation).await,
                () = cancel.cancelled() => {}
            }
        });
    }

    fn disarm_watchdog(&self) {
        let mut watchdog = self.watchdog.lock();
        if let Some(cancel) = watchdog.cancel.take() {
            cancel.cancel();
        }
        watchdog.generation += 1;
    }

    async fn thaw_for_watchdog(self: &Arc<Self>, generation: u64) {
        let permit = self.lock.acquire().await.expect("the freezer lock is never closed");
        if self.watchdog.lock().generation != generation {
            return;
        }
        let (result, errors) = self.unfreeze_locked(THAW_MAX_CGROUPS);
        if !errors.is_empty() || result.truncated || result.failed > 0 {
            self.arm_watchdog();
        }
        drop(permit);
        let error = (!errors.is_empty()).then(|| errors.join("\n"));
        tracing::warn!(
            thawed = result.thawed,
            visited = result.visited,
            failed = result.failed,
            truncated = result.truncated,
            error = error.as_deref(),
            "thaw watchdog fired: a freeze was never followed by a thaw"
        );
    }
}

fn record_vanished(result: &mut FreezeResult, path: &Path) {
    result.vanished += 1;
    sample_path(&mut result.vanished_paths, path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroups::spawn::SpawnRefused;

    struct Tree {
        dir: tempfile::TempDir,
        proc_file: PathBuf,
    }

    impl Tree {
        fn new(own: &str, cgroups: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            cgroups.iter().for_each(|cgroup| {
                let path = dir.path().join(cgroup);
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("cgroup.freeze"), "0").unwrap();
                std::fs::write(path.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
            });
            let proc_file = dir.path().join("proc-self-cgroup");
            std::fs::write(&proc_file, format!("0::/{own}\n")).unwrap();
            Self { dir, proc_file }
        }

        fn path(&self, cgroup: &str) -> PathBuf {
            self.dir.path().join(cgroup)
        }

        fn set_frozen(&self, cgroup: &str) {
            std::fs::write(self.path(cgroup).join("cgroup.events"), "populated 1\nfrozen 1\n").unwrap();
        }

        fn requested(&self, cgroup: &str) -> bool {
            std::fs::read_to_string(self.path(cgroup).join("cgroup.freeze")).unwrap() == "1"
        }

        fn freezer(&self) -> Arc<WorkloadFreezer> {
            self.freezer_with_user_at("user")
        }

        fn freezer_with_user_at(&self, user: &str) -> Arc<WorkloadFreezer> {
            self.freezer_configured(user, Duration::ZERO, SpawnBarrier::default())
        }

        fn freezer_with_barrier(&self, barrier: SpawnBarrier) -> Arc<WorkloadFreezer> {
            self.freezer_configured("user", Duration::ZERO, barrier)
        }

        fn freezer_configured(&self, user: &str, window: Duration, spawns: SpawnBarrier) -> Arc<WorkloadFreezer> {
            let manager = CgroupManager {
                root: self.dir.path().to_path_buf(),
                cgroups: [
                    (ProcessType::User, user),
                    (ProcessType::Pty, "ptys"),
                    (ProcessType::Socat, "socats"),
                ]
                .into_iter()
                .map(|(process_type, name)| {
                    let path = self.path(name);
                    std::fs::write(path.join("cgroup.procs"), "").unwrap();
                    let procs = std::fs::OpenOptions::new()
                        .write(true)
                        .open(path.join("cgroup.procs"))
                        .unwrap();
                    (
                        process_type,
                        super::super::Cgroup {
                            path,
                            procs: Arc::new(procs),
                        },
                    )
                })
                .collect(),
            };
            WorkloadFreezer::with(Arc::new(manager), window, self.proc_file.clone(), spawns)
        }
    }

    const CGROUPS: [&str; 8] = [
        "user",
        "ptys",
        "socats",
        "init.scope",
        "system.slice",
        "system.slice/envd.service",
        "system.slice/docker.service",
        "guest.slice",
    ];

    fn options(mode: FreezeMode, max_wait: Duration) -> FreezeOptions {
        FreezeOptions {
            max_wait,
            mode,
            max_cgroups: 0,
        }
    }

    #[tokio::test]
    async fn legacy_mode_freezes_only_the_workload_cgroups() {
        let tree = Tree::new("system.slice/envd.service", &CGROUPS);
        let freezer = tree.freezer();

        let (result, errors) = freezer.freeze(options(FreezeMode::Legacy, Duration::ZERO)).await;

        assert!(errors.is_empty());
        assert_eq!(
            (result.mode, result.requested, result.not_frozen),
            (FreezeMode::Legacy, 2, 2)
        );
        assert!(tree.requested("user") && tree.requested("ptys"));
        assert!(!tree.requested("system.slice/docker.service"));
    }

    #[tokio::test]
    async fn hierarchy_mode_freezes_everything_outside_envd_and_the_allowlist() {
        let tree = Tree::new("system.slice/envd.service", &CGROUPS);
        tree.set_frozen("system.slice/docker.service");
        std::fs::write(tree.path("guest.slice/cgroup.freeze"), "1").unwrap();
        let freezer = tree.freezer();

        let (result, errors) = freezer
            .freeze(options(FreezeMode::Hierarchy, Duration::from_millis(20)))
            .await;

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(result.mode, FreezeMode::Hierarchy);
        assert_eq!((result.requested, result.frozen, result.not_frozen), (3, 1, 2));
        assert_eq!((result.pre_frozen, result.allowlisted), (1, 2));
        assert!(tree.requested("system.slice/docker.service"));
        assert!(!tree.requested("system.slice/envd.service"));
        assert!(!tree.requested("socats") && !tree.requested("init.scope"));

        let audit = freezer.audit().unwrap().unwrap();
        assert!(audit.applicable);

        let thawed = freezer.unfreeze().await.unwrap();
        assert!(thawed.discovered);
        assert_eq!((thawed.thawed, thawed.preserved), (1, 1));
        assert!(!tree.requested("system.slice/docker.service"));
        assert!(tree.requested("guest.slice"));
        assert!(!tree.requested("user"));
    }

    #[tokio::test]
    async fn a_no_op_manager_reports_unobservable_and_audits_nothing() {
        let freezer = WorkloadFreezer::with(
            Arc::default(),
            Duration::ZERO,
            PathBuf::from(PROC_SELF_CGROUP),
            SpawnBarrier::default(),
        );

        let (result, errors) = freezer
            .freeze(options(FreezeMode::Hierarchy, Duration::from_millis(5)))
            .await;

        assert!(errors.is_empty());
        assert_eq!((result.mode, result.requested), (FreezeMode::Legacy, 2));
        assert_eq!((result.unobservable, result.not_frozen), (2, 0));
        assert!(freezer.audit().is_none());
        assert_eq!(freezer.unfreeze().await, Ok(ThawResult::default()));
    }

    #[test]
    fn max_cgroups_defaults_and_caps_like_upstream() {
        let with = |max_cgroups| FreezeOptions {
            max_wait: Duration::ZERO,
            mode: FreezeMode::Legacy,
            max_cgroups,
        };
        assert_eq!(with(0).max_cgroups(), 512);
        assert_eq!(with(-3).max_cgroups(), 512);
        assert_eq!(with(20_000).max_cgroups(), 8192);
        assert_eq!(with(40).max_cgroups(), 40);
    }

    fn noop_freezer() -> Arc<WorkloadFreezer> {
        WorkloadFreezer::with(
            Arc::default(),
            Duration::ZERO,
            PathBuf::from(PROC_SELF_CGROUP),
            SpawnBarrier::default(),
        )
    }

    fn no_wait() -> FreezeOptions {
        options(FreezeMode::Legacy, Duration::ZERO)
    }

    fn envd_tree() -> Tree {
        Tree::new("envd.service", &["user", "ptys", "socats", "envd.service"])
    }

    fn break_freeze_file(tree: &Tree, cgroup: &str) {
        let file = tree.path(cgroup).join("cgroup.freeze");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
    }

    async fn drain_queued(barrier: &SpawnBarrier) {
        for _ in 0..1000 {
            if barrier.drain_is_queued() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("the raise never queued its drain");
    }

    #[tokio::test]
    async fn a_spawn_is_refused_for_the_whole_frozen_window() {
        let freezer = noop_freezer();

        freezer
            .begin_spawn(ProcessType::User)
            .await
            .expect("no freeze in effect");

        freezer.freeze(no_wait()).await;
        assert_eq!(
            freezer.begin_spawn(ProcessType::User).await.err(),
            Some(SpawnRefused::WorkloadFrozen(None)),
            "the sweep has returned but the workload is frozen until the resume thaw"
        );

        freezer.unfreeze().await.unwrap();
        freezer
            .begin_spawn(ProcessType::User)
            .await
            .expect("the thaw must re-admit spawns");
    }

    #[tokio::test]
    async fn the_refusal_is_prompt_rather_than_a_timeout() {
        let tree = envd_tree();
        let freezer = tree.freezer_with_barrier(SpawnBarrier::with_waits(
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        ));
        freezer.freeze(no_wait()).await;

        let refused = tokio::time::timeout(Duration::from_secs(5), freezer.begin_spawn(ProcessType::User))
            .await
            .expect("the spawn waited on the semaphore instead of being refused by the held barrier");

        assert_eq!(refused.err(), Some(SpawnRefused::WorkloadFrozen(None)));
    }

    #[tokio::test]
    async fn a_freeze_drains_an_in_flight_spawn_before_writing() {
        let tree = envd_tree();
        let freezer = tree.freezer_with_barrier(SpawnBarrier::with_waits(
            Duration::from_secs(3600),
            Duration::from_millis(100),
        ));
        let in_flight = freezer.begin_spawn(ProcessType::User).await.unwrap();

        let freezing = tokio::spawn({
            let freezer = freezer.clone();
            async move { freezer.freeze(no_wait()).await }
        });
        drain_queued(&freezer.spawns).await;
        assert!(
            !tree.requested("user"),
            "the sweep wrote cgroup.freeze while a spawn was still in flight"
        );
        assert!(
            !freezing.is_finished(),
            "the freeze completed without waiting for the spawn"
        );

        drop(in_flight);

        let (result, errors) = tokio::time::timeout(Duration::from_secs(2), freezing)
            .await
            .expect("the freeze did not proceed after the spawn returned")
            .unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(result.requested, 2);
        assert!(tree.requested("user"));
    }

    #[tokio::test]
    async fn a_freeze_aborts_rather_than_freeze_over_a_wedged_spawn() {
        let tree = envd_tree();
        let freezer = tree.freezer();
        let wedged = freezer.begin_spawn(ProcessType::User).await.unwrap();

        let (result, errors) = freezer.freeze(no_wait()).await;

        assert_eq!(
            errors,
            vec!["timed out waiting for an in-flight process spawn".to_owned()]
        );
        assert_eq!(result.requested, 0, "nothing may be requested when the drain failed");
        assert!(
            !result.all_frozen(),
            "a freeze that froze nothing must not read as a workload that stopped"
        );
        assert!(!tree.requested("user") && !tree.requested("ptys"));
        drop(wedged);
    }

    #[tokio::test]
    async fn the_probe_refuses_a_freeze_the_guest_issued() {
        let tree = envd_tree();
        tree.set_frozen("user");
        let freezer = tree.freezer();

        let refused = freezer.begin_spawn(ProcessType::User).await.err().unwrap();

        assert_eq!(
            refused,
            SpawnRefused::WorkloadFrozen(Some(format!("{} reads frozen", tree.path("user").display())))
        );
        freezer
            .begin_spawn(ProcessType::Pty)
            .await
            .expect("a cgroup nobody froze is unaffected");
    }

    #[tokio::test]
    async fn the_probe_refuses_an_unsettled_ancestor_freeze() {
        let tree = Tree::new("envd.service", &["customer/nested", "ptys", "socats", "envd.service"]);
        let freezer = tree.freezer_with_user_at("customer/nested");
        std::fs::write(tree.path("customer").join("cgroup.freeze"), "1\n").unwrap();
        assert_eq!(frozen_at(&tree.path("customer/nested")).ok(), Some(false));

        let refused = freezer.begin_spawn(ProcessType::User).await.err().unwrap();

        assert_eq!(
            refused,
            SpawnRefused::WorkloadFrozen(Some(format!(
                "{} has a freeze requested",
                tree.path("customer").display()
            )))
        );
    }

    #[tokio::test]
    async fn the_probe_tolerates_unreadable_state() {
        let tree = envd_tree();
        std::fs::remove_file(tree.path("user").join("cgroup.events")).unwrap();
        std::fs::remove_file(tree.path("user").join("cgroup.freeze")).unwrap();

        tree.freezer()
            .begin_spawn(ProcessType::User)
            .await
            .expect("an unreadable cgroupfs must not become a process-start outage");
    }

    #[tokio::test]
    async fn non_workload_cgroups_skip_the_barrier() {
        let freezer = noop_freezer();
        freezer.freeze(no_wait()).await;

        for process_type in [ProcessType::Socat, ProcessType::System] {
            freezer
                .begin_spawn(process_type)
                .await
                .unwrap_or_else(|e| panic!("{process_type:?} is not a cgroup a sweep freezes: {e}"));
        }
    }

    #[tokio::test]
    async fn the_probe_still_covers_non_workload_cgroups() {
        let tree = envd_tree();
        tree.set_frozen("socats");

        let refused = tree.freezer().begin_spawn(ProcessType::Socat).await;

        assert!(matches!(refused, Err(SpawnRefused::WorkloadFrozen(Some(_)))));
    }

    #[tokio::test]
    async fn concurrent_spawns_are_admitted_and_give_their_weight_back() {
        let freezer = noop_freezer();

        let mut admissions = Vec::new();
        for _ in 0..32 {
            admissions.push(freezer.begin_spawn(ProcessType::User).await.unwrap());
        }
        drop(admissions);

        let (_, errors) = freezer.freeze(no_wait()).await;
        assert!(
            errors.is_empty(),
            "a spawn's weight was not returned, so the drain could not complete"
        );
    }

    #[tokio::test]
    async fn a_sweep_that_froze_nothing_admits_spawns() {
        let tree = envd_tree();
        break_freeze_file(&tree, "user");
        break_freeze_file(&tree, "ptys");
        let freezer = tree.freezer();

        let (result, errors) = freezer.freeze(no_wait()).await;
        assert_eq!((result.requested, errors.len()), (0, 2));

        freezer
            .begin_spawn(ProcessType::User)
            .await
            .expect("a sweep whose writes all failed opened no window");
    }

    #[tokio::test]
    async fn a_dirty_thaw_keeps_refusing_spawns() {
        let tree = envd_tree();
        let freezer = tree.freezer();
        freezer.freeze(no_wait()).await;
        break_freeze_file(&tree, "user");
        break_freeze_file(&tree, "ptys");

        assert!(freezer.unfreeze().await.is_err());

        assert_eq!(
            freezer.begin_spawn(ProcessType::User).await.err(),
            Some(SpawnRefused::WorkloadFrozen(None))
        );
    }

    #[tokio::test]
    async fn resuming_a_frozen_workload_raises_the_barrier() {
        let freezer = noop_freezer();

        freezer.resume_frozen().await.unwrap();

        assert_eq!(
            freezer.begin_spawn(ProcessType::User).await.err(),
            Some(SpawnRefused::WorkloadFrozen(None))
        );

        freezer.unfreeze().await.unwrap();
        freezer
            .begin_spawn(ProcessType::User)
            .await
            .expect("the post-upgrade /init thaw must re-admit spawns");
    }

    #[tokio::test]
    async fn a_second_freeze_inside_the_same_window_keeps_the_barrier() {
        let freezer = noop_freezer();
        freezer.freeze(no_wait()).await;

        let (hold, _, errors) = freezer.freeze_hold(no_wait()).await;
        assert!(
            errors.is_empty(),
            "a second freeze inside the same window drained against the first"
        );
        drop(hold);

        assert_eq!(
            freezer.begin_spawn(ProcessType::User).await.err(),
            Some(SpawnRefused::WorkloadFrozen(None)),
            "releasing the second freeze's hold must not lower the barrier"
        );
    }

    #[tokio::test]
    async fn a_vanished_static_cgroup_does_not_hold_the_barrier() {
        let tree = envd_tree();
        let freezer = tree.freezer();
        std::fs::remove_file(tree.path("ptys").join("cgroup.freeze")).unwrap();

        let (result, errors) = freezer.freeze(no_wait()).await;
        assert_eq!(
            (result.requested, errors.len()),
            (1, 1),
            "the user freeze must still open a window"
        );
        assert_eq!(
            freezer
                .begin_spawn(ProcessType::User)
                .await
                .err()
                .map(|e| matches!(e, SpawnRefused::WorkloadFrozen(_))),
            Some(true),
            "the window is open until the thaw"
        );

        assert!(freezer.unfreeze().await.is_err(), "the pty thaw fails as well");

        freezer
            .begin_spawn(ProcessType::User)
            .await
            .expect("user was thawed and pty no longer exists, so a start must be admitted");
    }

    #[tokio::test]
    async fn a_partial_thaw_of_the_guest_readmits_spawns() {
        let tree = Tree::new("envd.service", &["user", "ptys", "socats", "envd.service", "customer"]);
        let freezer = tree.freezer_configured("user", Duration::from_secs(3600), SpawnBarrier::default());
        let (result, errors) = freezer.freeze(options(FreezeMode::Hierarchy, Duration::ZERO)).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            tree.requested("customer"),
            "the fixture must model a guest cgroup we froze"
        );
        assert!(result.requested > 0);

        let permit = freezer.lock.clone().acquire_owned().await.unwrap();
        let (thaw, thaw_errors) = freezer.unfreeze_locked(1);
        drop(permit);

        assert!(!thaw_errors.is_empty());
        assert!(thaw.truncated);
        assert!(tree.requested("customer"));
        freezer
            .begin_spawn(ProcessType::User)
            .await
            .expect("user was thawed, so a start into it must be admitted");
        assert!(
            freezer.freeze_in_effect(),
            "a dirty thaw keeps the freeze counted as in effect"
        );
        assert!(
            freezer.watchdog.lock().cancel.is_some(),
            "a dirty thaw keeps the watchdog armed"
        );
    }
}
