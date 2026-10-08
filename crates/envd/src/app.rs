use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use parking_lot::RwLock;

use crate::cgroups::CgroupManager;
use crate::cgroups::freeze::WorkloadFreezer;
use crate::config::Config;
use crate::envvars::EnvVars;
use crate::host::metrics::{CpuUsage, read_metrics};
use crate::host::oom::OomWatcher;

#[derive(Debug, Clone)]
pub struct Defaults {
    pub user: String,
    pub workdir: Option<String>,
    pub user_delivered: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            user: "root".into(),
            workdir: None,
            user_delivered: false,
        }
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
    pub cpu: CpuUsage,
    pub freezer: Arc<WorkloadFreezer>,
    pub fs_freeze_lock: tokio::sync::Mutex<()>,
    pub oom_kills: Option<Arc<OomWatcher>>,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        let cpu = CpuUsage::start();
        let cgroups = CgroupManager::create(&config, || read_metrics(&cpu).map(|metrics| metrics.mem_total));
        let freezer = WorkloadFreezer::new(Arc::new(cgroups));
        Arc::new(Self {
            oom_kills: (!config.is_not_fc).then(OomWatcher::start),
            cpu,
            freezer,
            fs_freeze_lock: tokio::sync::Mutex::new(()),
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
        self.defaults.read().clone()
    }

    pub fn access_token(&self) -> Option<String> {
        self.access_token.read().clone()
    }
}
