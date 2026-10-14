use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};

use parking_lot::{Mutex, RwLock};

use crate::cgroups::CgroupManager;
use crate::cgroups::freeze::WorkloadFreezer;
use crate::cgroups::hierarchy::PROC_SELF_CGROUP;
use crate::cgroups::memory::MemoryProtection;
use crate::config::Config;
use crate::controllers::filesystem::watch::FileWatcher;
use crate::controllers::process::registry::Processes;
use crate::cpus::hotplug::Hotplug;
use crate::cpus::sysfs::KernelSysfs;
use crate::cpus::{CpuManager, NoopCpus};
use crate::envvars::EnvVars;
use crate::host::cacerts::CaCertInstaller;
use crate::host::metrics::{CpuUsage, read_metrics};
use crate::host::mmds::MmdsOpts;
use crate::host::nfs::NfsMounts;
use crate::host::oom::OomWatcher;
use crate::host::{GuestClock, SystemClock};
use crate::logs::exporter::LogExporter;
use crate::port::forward::Forwards;
use crate::reaper::Registry;
use crate::upgrade::HandoverReport;

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
    pub memory: MemoryProtection,
    pub ca_certs: CaCertInstaller,
    pub nfs: NfsMounts,
    pub hyperloop_lock: Mutex<()>,
    pub log_exporter: Option<Arc<LogExporter>>,
    pub processes: Arc<Processes>,
    pub watchers: Arc<Mutex<HashMap<String, FileWatcher>>>,
    pub forwards: Forwards,
    pub handover: Mutex<Option<HandoverReport>>,
    pub children: Arc<Registry>,
    pub cpus: Arc<dyn CpuManager>,
    pub clock: Arc<dyn GuestClock>,
}

pub struct Seams {
    pub children: Arc<Registry>,
    pub cpus: Arc<dyn CpuManager>,
    pub clock: Arc<dyn GuestClock>,
}

impl Seams {
    pub fn for_config(config: &Config) -> Self {
        let children = Arc::new(Registry::default());
        let cpus: Arc<dyn CpuManager> = match config.is_not_fc {
            true => Arc::new(NoopCpus),
            false => Hotplug::builder()
                .sys(KernelSysfs::builder().children(children.clone()).build())
                .children(children.clone())
                .build(),
        };
        Self {
            children,
            cpus,
            clock: Arc::new(SystemClock),
        }
    }
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        let seams = Seams::for_config(&config);
        Self::with_seams(config, seams)
    }

    pub fn with_seams(config: Config, seams: Seams) -> Arc<Self> {
        let Seams { children, cpus, clock } = seams;
        let cpu = CpuUsage::start();
        let cgroups = CgroupManager::create(&config, || read_metrics(&cpu).map(|metrics| metrics.mem_total));
        let memory = MemoryProtection::read(Path::new(PROC_SELF_CGROUP), cgroups.walk_root());
        let freezer = WorkloadFreezer::new(Arc::new(cgroups));
        Arc::new(Self {
            memory,
            ca_certs: CaCertInstaller::default(),
            nfs: NfsMounts::default(),
            hyperloop_lock: Mutex::new(()),
            log_exporter: (!config.is_not_fc).then(Arc::default),
            processes: Arc::default(),
            watchers: Arc::default(),
            forwards: Forwards::default(),
            handover: Mutex::new(None),
            children,
            cpus,
            clock,
            oom_kills: (!config.is_not_fc).then(OomWatcher::start),
            cpu,
            freezer,
            fs_freeze_lock: tokio::sync::Mutex::new(()),
            config,
            defaults: RwLock::default(),
            env_vars: EnvVars::default(),
            access_token: RwLock::new(None),
            initialized: AtomicBool::new(false),
            last_set_time: AtomicI64::new(0),
            init_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn defaults(&self) -> Defaults {
        self.defaults.read().clone()
    }

    pub fn deliver_mmds(&self, opts: Option<MmdsOpts>) {
        if let (Some(exporter), Some(opts)) = (&self.log_exporter, opts)
            && !opts.logs_collector_address.is_empty()
        {
            exporter.set_opts(opts);
        }
    }

    pub async fn flush_logs(&self, budget: std::time::Duration) -> Result<(), String> {
        match &self.log_exporter {
            Some(exporter) => exporter.flush_and_purge(budget).await,
            None => Ok(()),
        }
    }

    pub fn access_token(&self) -> Option<String> {
        self.access_token.read().clone()
    }
}
