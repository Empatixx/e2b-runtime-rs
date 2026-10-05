use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, RwLock};

use crate::config::Config;
use crate::execcontext::BUILTIN_DEFAULT_USER;
use crate::host::mmds::{HttpMmds, Mmds};
use crate::utils::envvars::EnvVars;

/// Values delivered by `/init`, used as defaults for every process / file operation.
#[derive(Debug, Clone)]
pub struct Defaults {
    pub user: String,
    pub workdir: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self { user: BUILTIN_DEFAULT_USER.into(), workdir: None }
    }
}

pub struct AppState {
    pub config: Config,
    pub defaults: RwLock<Defaults>,
    pub env_vars: EnvVars,
    pub access_token: RwLock<Option<String>>,
    pub initialized: AtomicBool,
    /// Newest `/init` timestamp applied (unix nanos); older `/init`s don't overwrite data.
    pub last_set_time: AtomicI64,
    /// Serialises `/init` requests.
    pub init_lock: tokio::sync::Mutex<()>,
    pub mmds: Arc<dyn Mmds>,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Self::with_mmds(config, Arc::new(HttpMmds::default()))
    }

    pub fn with_mmds(config: Config, mmds: Arc<dyn Mmds>) -> Arc<Self> {
        Arc::new(Self {
            config,
            defaults: RwLock::default(),
            env_vars: EnvVars::default(),
            access_token: RwLock::new(None),
            initialized: AtomicBool::new(false),
            last_set_time: AtomicI64::new(i64::MIN),
            init_lock: tokio::sync::Mutex::new(()),
            mmds,
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
