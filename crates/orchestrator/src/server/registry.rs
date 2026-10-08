use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;

const ALREADY_RUNNING: &str = "sandbox is already running on this node";
const CLEANUP_STARTED: &str = "sandbox cleanup has already started";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReserved {
    InProgress,
    NotLive,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Lifecycle {
    pub sandbox_id: String,
    pub lifecycle_id: String,
    pub execution_id: String,
}

struct Live<T> {
    lifecycle: Lifecycle,
    value: T,
}

struct Entries<T> {
    live: HashMap<String, Live<T>>,
    reserved: HashSet<String>,
    stopping: HashSet<Lifecycle>,
}

pub struct Registry<T> {
    entries: Mutex<Entries<T>>,
}

impl<T: Clone> Registry<T> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Entries {
                live: HashMap::new(),
                reserved: HashSet::new(),
                stopping: HashSet::new(),
            }),
        }
    }

    pub fn reserve(&self, id: &str) -> Result<(), String> {
        let mut entries = self.entries.lock();
        if let Some(live) = entries.live.get(id) {
            return Err(already_running(
                id,
                &format!(
                    "lifecycle {} (execution {}) is live",
                    live.lifecycle.lifecycle_id, live.lifecycle.execution_id
                ),
            ));
        }
        if !entries.reserved.insert(id.to_owned()) {
            return Err(already_running(id, "another create is in flight"));
        }
        Ok(())
    }

    pub fn activate(
        &self,
        lifecycle: Lifecycle,
        value: T,
        cleanup_started: impl FnOnce() -> bool,
    ) -> Result<(), String> {
        let mut entries = self.entries.lock();
        if let Some(live) = entries.live.get(&lifecycle.sandbox_id) {
            return match live.lifecycle.lifecycle_id == lifecycle.lifecycle_id {
                true => Ok(()),
                false => Err(format!(
                    "{ALREADY_RUNNING}: lifecycle {} (execution {}) is live",
                    live.lifecycle.lifecycle_id, live.lifecycle.execution_id
                )),
            };
        }
        if cleanup_started() {
            return Err(CLEANUP_STARTED.to_owned());
        }
        entries
            .live
            .insert(lifecycle.sandbox_id.clone(), Live { lifecycle, value });
        Ok(())
    }

    pub fn release(&self, id: &str) {
        self.entries.lock().reserved.remove(id);
    }

    pub fn get(&self, id: &str) -> Option<T> {
        self.entries.lock().live.get(id).map(|live| live.value.clone())
    }

    pub fn stop(&self, lifecycle: &Lifecycle) -> Option<T> {
        let mut entries = self.entries.lock();
        entries.take_live(lifecycle)
    }

    pub fn stop_reserved(&self, lifecycle: &Lifecycle) -> Result<T, StopReserved> {
        let mut entries = self.entries.lock();
        if !entries.is_live(lifecycle) {
            return Err(StopReserved::NotLive);
        }
        if entries.reserved.contains(&lifecycle.sandbox_id) {
            return Err(StopReserved::InProgress);
        }
        entries.reserved.insert(lifecycle.sandbox_id.clone());
        entries.take_live(lifecycle).ok_or(StopReserved::NotLive)
    }

    pub fn finish(&self, lifecycle: &Lifecycle) {
        self.entries.lock().stopping.remove(lifecycle);
    }

    pub fn running(&self) -> Vec<T> {
        self.entries
            .lock()
            .live
            .values()
            .map(|live| live.value.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        let entries = self.entries.lock();
        entries.live.is_empty() && entries.reserved.is_empty() && entries.stopping.is_empty()
    }
}

impl<T> Entries<T> {
    fn is_live(&self, lifecycle: &Lifecycle) -> bool {
        self.live
            .get(&lifecycle.sandbox_id)
            .is_some_and(|live| live.lifecycle.lifecycle_id == lifecycle.lifecycle_id)
    }

    fn take_live(&mut self, lifecycle: &Lifecycle) -> Option<T> {
        self.is_live(lifecycle).then_some(())?;
        let live = self.live.remove(&lifecycle.sandbox_id)?;
        self.stopping.insert(live.lifecycle);
        Some(live.value)
    }
}

impl<T: Clone> Default for Registry<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn already_running(id: &str, reason: &str) -> String {
    format!("sandbox '{id}' is already running on this node: {ALREADY_RUNNING}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle(id: &str, lifecycle_id: &str) -> Lifecycle {
        Lifecycle {
            sandbox_id: id.to_owned(),
            lifecycle_id: lifecycle_id.to_owned(),
            execution_id: "exec".to_owned(),
        }
    }

    fn started(registry: &Registry<i32>, lifecycle_id: &str, value: i32) {
        registry.reserve("sbx").unwrap();
        registry
            .activate(lifecycle("sbx", lifecycle_id), value, || false)
            .unwrap();
        registry.release("sbx");
    }

    #[test]
    fn an_id_is_reserved_once_and_listed_only_while_live() {
        let registry = Registry::new();

        registry.reserve("sbx").unwrap();
        assert_eq!(
            registry.reserve("sbx").unwrap_err(),
            "sandbox 'sbx' is already running on this node: sandbox is already running on this node: another create is in flight"
        );
        assert!(registry.running().is_empty());
        assert_eq!(registry.stop(&lifecycle("sbx", "l1")), None);

        registry.activate(lifecycle("sbx", "l1"), 7, || false).unwrap();
        registry.release("sbx");
        assert_eq!(registry.running(), vec![7]);
        assert_eq!(registry.get("sbx"), Some(7));
        assert_eq!(
            registry.reserve("sbx").unwrap_err(),
            "sandbox 'sbx' is already running on this node: sandbox is already running on this node: lifecycle l1 (execution exec) is live"
        );
    }

    #[test]
    fn a_stopping_sandbox_frees_its_id_but_keeps_the_node_busy_until_teardown() {
        let registry = Registry::new();
        started(&registry, "l1", 7);

        assert_eq!(registry.stop(&lifecycle("sbx", "l1")), Some(7));
        assert_eq!(registry.stop(&lifecycle("sbx", "l1")), None);
        started(&registry, "l2", 8);

        registry.finish(&lifecycle("sbx", "l1"));
        assert_eq!(registry.get("sbx"), Some(8));
        assert_eq!(registry.stop(&lifecycle("sbx", "l1")), None);
        assert_eq!(registry.get("sbx"), Some(8));
        registry.stop(&lifecycle("sbx", "l2"));
        assert!(!registry.is_empty());
        registry.finish(&lifecycle("sbx", "l2"));
        assert!(registry.is_empty());
    }

    #[test]
    fn a_reservation_is_held_until_released() {
        let registry = Registry::new();
        registry.reserve("sbx").unwrap();
        registry.release("sbx");
        registry.reserve("sbx").unwrap();
        registry.activate(lifecycle("sbx", "l1"), 7, || false).unwrap();
        registry.stop(&lifecycle("sbx", "l1"));
        registry.finish(&lifecycle("sbx", "l1"));

        assert!(!registry.is_empty());
        assert!(registry.reserve("sbx").is_err());
        registry.release("sbx");
        assert!(registry.is_empty());
    }

    #[test]
    fn a_sandbox_whose_cleanup_started_or_whose_id_is_live_is_not_registered() {
        let registry = Registry::new();
        registry.reserve("sbx").unwrap();

        assert_eq!(
            registry.activate(lifecycle("sbx", "l1"), 7, || true),
            Err("sandbox cleanup has already started".to_owned())
        );
        assert!(registry.get("sbx").is_none());
        registry.activate(lifecycle("sbx", "l1"), 7, || false).unwrap();
        assert_eq!(registry.activate(lifecycle("sbx", "l1"), 7, || true), Ok(()));
        assert_eq!(
            registry.activate(lifecycle("sbx", "l2"), 8, || false),
            Err("sandbox is already running on this node: lifecycle l1 (execution exec) is live".to_owned())
        );
        assert_eq!(registry.get("sbx"), Some(7));
    }

    #[test]
    fn a_checkpoint_holds_the_id_until_it_releases_it() {
        let registry = Registry::new();
        started(&registry, "l1", 7);

        assert_eq!(
            registry.stop_reserved(&lifecycle("sbx", "l0")),
            Err(StopReserved::NotLive)
        );
        assert_eq!(registry.stop_reserved(&lifecycle("sbx", "l1")), Ok(7));
        assert!(registry.get("sbx").is_none());
        assert!(registry.reserve("sbx").is_err());
        registry.activate(lifecycle("sbx", "l2"), 8, || false).unwrap();
        assert_eq!(registry.get("sbx"), Some(8));
        assert_eq!(
            registry.stop_reserved(&lifecycle("sbx", "l2")),
            Err(StopReserved::InProgress)
        );
        assert_eq!(registry.get("sbx"), Some(8));

        registry.release("sbx");
        assert_eq!(registry.stop_reserved(&lifecycle("sbx", "l2")), Ok(8));
    }
}
