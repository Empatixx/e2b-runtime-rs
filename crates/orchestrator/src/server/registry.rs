use std::collections::HashMap;

use parking_lot::Mutex;

pub struct Registry<T> {
    entries: Mutex<HashMap<String, Option<T>>>,
}

impl<T: Clone> Registry<T> {
    pub fn new() -> Self {
        Self {
            entries: Mutex::default(),
        }
    }

    pub fn reserve(&self, id: &str) -> Result<(), String> {
        let mut entries = self.entries.lock();
        if entries.contains_key(id) {
            return Err(format!("sandbox '{id}' is already running on this node: id is taken"));
        }
        entries.insert(id.to_owned(), None);
        Ok(())
    }

    pub fn activate(&self, id: &str, value: T) {
        self.entries.lock().insert(id.to_owned(), Some(value));
    }

    pub fn get(&self, id: &str) -> Option<T> {
        self.entries.lock().get(id).cloned().flatten()
    }

    pub fn remove(&self, id: &str) -> Option<T> {
        self.entries.lock().remove(id).flatten()
    }

    pub fn running(&self) -> Vec<T> {
        self.entries.lock().values().flatten().cloned().collect()
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
    fn an_id_is_reserved_once_and_listed_only_when_active() {
        let registry = Registry::new();

        registry.reserve("sbx").unwrap();
        assert_eq!(
            registry.reserve("sbx").unwrap_err(),
            "sandbox 'sbx' is already running on this node: id is taken"
        );
        assert!(registry.running().is_empty());
        assert_eq!(registry.get("sbx"), None);

        registry.activate("sbx", 7);
        assert_eq!(registry.running(), vec![7]);
        assert_eq!(registry.remove("sbx"), Some(7));
        assert_eq!(registry.remove("sbx"), None);
        registry.reserve("sbx").unwrap();
    }
}
