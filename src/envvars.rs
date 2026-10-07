use std::collections::BTreeMap;
use std::sync::{PoisonError, RwLock};

#[derive(Debug, Clone)]
struct Var {
    value: String,
    internal: bool,
}

#[derive(Debug, Default)]
pub struct EnvVars {
    vars: RwLock<BTreeMap<String, Var>>,
}

impl EnvVars {
    pub fn store(&self, key: &str, value: &str) {
        self.vars.write().unwrap_or_else(PoisonError::into_inner).insert(
            key.to_owned(),
            Var {
                value: value.to_owned(),
                internal: true,
            },
        );
    }

    pub fn all(&self) -> BTreeMap<String, String> {
        self.vars
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(key, var)| (key.clone(), var.value.clone()))
            .collect()
    }

    pub fn replace_user_vars(&self, vars: &BTreeMap<String, String>) {
        let mut map = self.vars.write().unwrap_or_else(PoisonError::into_inner);
        map.retain(|key, var| var.internal || vars.contains_key(key));
        for (key, value) in vars {
            if !map.get(key).is_some_and(|var| var.internal) {
                map.insert(
                    key.clone(),
                    Var {
                        value: value.clone(),
                        internal: false,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn replace_keeps_internal_and_drops_old_user_vars() {
        let env = EnvVars::default();
        env.store("E2B_SANDBOX_ID", "sbx");
        env.replace_user_vars(&vars(&[("A", "1"), ("B", "2")]));
        env.replace_user_vars(&vars(&[("B", "3"), ("E2B_SANDBOX_ID", "hacked")]));
        assert_eq!(env.all(), vars(&[("B", "3"), ("E2B_SANDBOX_ID", "sbx")]));
    }
}
