use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::checkpoint::Checkpoints;
use crate::config::Config;
use crate::layers::Layers;
use crate::network::{Network, run};
use crate::storage::Storage;
use crate::storage::shared_memory::SharedMemory;
use crate::store::Store;
use crate::template::Templates;
use crate::volume::Volumes;

const PLACEHOLDER_SIZE: &str = "1M";

pub struct Host {
    pub config: Config,
    pub store: Arc<Store>,
    pub layers: Arc<Layers>,
    pub storage: Arc<Storage>,
    pub memory: SharedMemory,
    pub network: Network,
    pub checkpoints: Checkpoints,
    pub templates: Templates,
    pub volumes: Volumes,
}

impl Host {
    pub async fn start(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        remove_stale_vms(&config.sandboxes_dir());
        Network::prepare_host().await?;
        create_placeholder_drive(&config).await?;
        let store = Arc::new(Store::new(&config.s3)?);
        let layers = Arc::new(Layers::new(store.clone(), config.layers_dir()));
        let storage = Arc::new(Storage::start(&config, layers.clone()).await?);
        Ok(Self {
            memory: SharedMemory::new(storage.clone(), config.memory_dir()),
            network: Network::new(config.slot_base),
            checkpoints: Checkpoints::new(store.clone(), config.checkpoints_dir()),
            templates: Templates::new(store.clone()),
            volumes: Volumes::new(store.clone(), storage.clone(), config.volumes_dir()),
            storage,
            layers,
            store,
            config,
        })
    }
}

fn remove_stale_vms(sandboxes_dir: &Path) {
    for pid in processes_working_in(sandboxes_dir) {
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = std::fs::remove_dir_all(sandboxes_dir);
}

fn processes_working_in(dir: &Path) -> Vec<i32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).is_ok_and(|cwd| cwd.starts_with(dir)))
        .collect()
}

async fn create_placeholder_drive(config: &Config) -> Result<()> {
    let path = config.placeholder_drive();
    match path.exists() {
        true => Ok(()),
        false => run("truncate", &["-s", PLACEHOLDER_SIZE, &path.display().to_string()]).await,
    }
}
