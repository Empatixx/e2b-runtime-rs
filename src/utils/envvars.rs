//! Global env vars for processes (upstream `utils/envvars.go`).
//! "Internal" vars come from MMDS (E2B_SANDBOX_ID, ...) and `/init` can never override or delete them.

use std::collections::BTreeMap;
use std::sync::RwLock;

#[derive(Debug, Clone)]
struct Entry {
    value: String,
    internal: bool,
}

#[derive(Debug, Default)]
pub struct EnvVars {
    map: RwLock<BTreeMap<String, Entry>>,
}

impl EnvVars {
    /// Internal var (from MMDS); always wins.
    pub fn store(&self, key: &str, value: &str) {
        self.write().insert(key.into(), Entry { value: value.into(), internal: true });
    }

    pub fn all(&self) -> BTreeMap<String, String> {
        self.read().iter().map(|(k, e)| (k.clone(), e.value.clone())).collect()
    }

    /// `/init` semantics: user vars become exactly `new_vars`, internal vars are untouched.
    pub fn replace_user_vars(&self, new_vars: &BTreeMap<String, String>) {
        let mut map = self.write();
        map.retain(|k, e| e.internal || new_vars.contains_key(k));
        for (k, v) in new_vars {
            if map.get(k).is_some_and(|e| e.internal) {
                continue;
            }
            map.insert(k.clone(), Entry { value: v.clone(), internal: false });
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Entry>> {
        self.map.read().expect("env vars lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Entry>> {
        self.map.write().expect("env vars lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn replace_keeps_internal_and_drops_old_user_vars() {
        let env = EnvVars::default();
        env.store("E2B_SANDBOX_ID", "sbx");
        env.replace_user_vars(&m(&[("A", "1"), ("B", "2")]));
        env.replace_user_vars(&m(&[("B", "3"), ("E2B_SANDBOX_ID", "hacked")]));
        assert_eq!(env.all(), m(&[("B", "3"), ("E2B_SANDBOX_ID", "sbx")]));
    }
}
