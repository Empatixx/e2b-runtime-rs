use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, PoisonError, RwLock};

use crate::config::Config;
use crate::envvars::EnvVars;

#[derive(Debug, Clone)]
pub struct Defaults {
    pub user: String,
    pub workdir: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self { user: "root".into(), workdir: None }
    }
}

pub struct AppState {
    pub config: Config,
    pub defaults: RwLock<Defaults>,
    pub env_vars: EnvVars,
    pub access_token: RwLock<Option<String>>,
    pub initialized: AtomicBool,
    pub last_set_time: AtomicI64,
    pub init_lock: tokio::sync::Mutex<()>,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            defaults: RwLock::default(),
            env_vars: EnvVars::default(),
            access_token: RwLock::new(None),
            initialized: AtomicBool::new(false),
            last_set_time: AtomicI64::new(i64::MIN),
            init_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn defaults(&self) -> Defaults {
        self.defaults.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn access_token(&self) -> Option<String> {
        self.access_token.read().unwrap_or_else(PoisonError::into_inner).clone()
    }
}
