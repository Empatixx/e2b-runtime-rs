use std::collections::HashMap;

use parking_lot::Mutex;

enum Entry<T> {
    Starting,
    Running(T),
    Stopping,
}

pub struct Registry<T> {
    entries: Mutex<HashMap<String, Entry<T>>>,
}

impl<T: Clone> Registry<T> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::default(),
        }
    }

    pub fn reserve(&self, id: &str) -> Result<(), String> {
        let mut entries = self.entries.lock();
        match entries.get(id) {
            Some(Entry::Starting) => Err(format!(
                "sandbox '{id}' is already running on this node: another create is in flight"
            )),
            Some(Entry::Running(_)) => Err(format!(
                "sandbox '{id}' is already running on this node: sandbox is already running"
            )),
            Some(Entry::Stopping) => Err(format!(
                "sandbox '{id}' is already running on this node: sandbox is still stopping"
            )),
            None => {
                entries.insert(id.to_owned(), Entry::Starting);
                Ok(())
            }
        }
    }

    pub fn activate(&self, id: &str, value: T) {
        self.entries.lock().insert(id.to_owned(), Entry::Running(value));
    }

    pub fn abandon(&self, id: &str) {
        let mut entries = self.entries.lock();
        if matches!(entries.get(id), Some(Entry::Starting)) {
            entries.remove(id);
        }
    }

    pub fn get(&self, id: &str) -> Option<T> {
        match self.entries.lock().get(id) {
            Some(Entry::Running(value)) => Some(value.clone()),
            _ => None,
        }
    }

    pub fn stop(&self, id: &str) -> Option<T> {
        let mut entries = self.entries.lock();
        match entries.remove(id) {
            Some(Entry::Running(value)) => {
                entries.insert(id.to_owned(), Entry::Stopping);
                Some(value)
            }
            Some(other) => {
                entries.insert(id.to_owned(), other);
                None
            }
            None => None,
        }
    }

    pub fn finish(&self, id: &str) {
        let mut entries = self.entries.lock();
        if matches!(entries.get(id), Some(Entry::Stopping)) {
            entries.remove(id);
        }
    }

    pub fn running(&self) -> Vec<T> {
        self.entries
            .lock()
            .values()
            .filter_map(|entry| match entry {
                Entry::Running(value) => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

impl<T: Clone> Default for Registry<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_reserved_once_and_listed_only_while_running() {
        let registry = Registry::new();

        registry.reserve("sbx").unwrap();
        assert_eq!(
            registry.reserve("sbx").unwrap_err(),
            "sandbox 'sbx' is already running on this node: another create is in flight"
        );
        assert!(registry.running().is_empty());
        assert_eq!(registry.stop("sbx"), None, "a starting sandbox cannot be stopped");

        registry.activate("sbx", 7);
        assert_eq!(registry.running(), vec![7]);
        assert_eq!(registry.get("sbx"), Some(7));
    }

    #[test]
    fn a_stopping_sandbox_keeps_its_id_until_teardown_finishes() {
        let registry = Registry::new();
        registry.reserve("sbx").unwrap();
        registry.activate("sbx", 7);

        assert_eq!(registry.stop("sbx"), Some(7));
        assert_eq!(registry.stop("sbx"), None);
        assert_eq!(registry.get("sbx"), None);
        assert!(registry.running().is_empty());
        assert_eq!(
            registry.reserve("sbx").unwrap_err(),
            "sandbox 'sbx' is already running on this node: sandbox is still stopping"
        );

        registry.finish("sbx");
        assert!(registry.is_empty());
        registry.reserve("sbx").unwrap();
    }

    #[test]
    fn abandon_and_finish_only_touch_their_own_state() {
        let registry = Registry::new();
        registry.reserve("sbx").unwrap();
        registry.finish("sbx");
        registry.reserve("sbx").unwrap_err();

        registry.abandon("sbx");
        registry.reserve("sbx").unwrap();
        registry.activate("sbx", 7);
        registry.abandon("sbx");
        assert_eq!(registry.get("sbx"), Some(7));
    }
}
