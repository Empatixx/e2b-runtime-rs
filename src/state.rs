use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use crate::config::Config;
use crate::execcontext::BUILTIN_DEFAULT_USER;

/// Values delivered by `/init`, used as defaults for every process / file operation.
#[derive(Debug, Clone)]
pub struct Defaults {
    pub env_vars: BTreeMap<String, String>,
    pub user: String,
    pub workdir: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            env_vars: BTreeMap::new(),
            user: BUILTIN_DEFAULT_USER.into(),
            workdir: None,
        }
    }
}

pub struct AppState {
    pub config: Config,
    pub defaults: RwLock<Defaults>,
    pub access_token: RwLock<Option<String>>,
    pub initialized: AtomicBool,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            defaults: RwLock::default(),
            access_token: RwLock::new(None),
            initialized: AtomicBool::new(false),
        })
    }

    /// Snapshot of the defaults (cloned, so the lock is held only briefly).
    pub fn defaults(&self) -> Defaults {
        self.defaults.read().expect("defaults lock poisoned").clone()
    }

    pub fn access_token(&self) -> Option<String> {
        self.access_token.read().expect("access token lock poisoned").clone()
    }
}
