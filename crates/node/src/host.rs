use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::network::{Network, run};
use crate::storage::Storage;
use crate::storage::shared_memory::SharedMemory;
use crate::volume::Volumes;

const PLACEHOLDER_SIZE: &str = "1M";

pub struct Host {
    pub config: Config,
    pub storage: Arc<Storage>,
    pub memory: SharedMemory,
    pub network: Network,
    pub volumes: Volumes,
}

impl Host {
    pub async fn start(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        Network::prepare_host().await?;
        create_placeholder_drive(&config).await?;
        let storage = Arc::new(Storage::start(&config).await?);
        Ok(Self {
            memory: SharedMemory::new(storage.clone(), config.memory_dir()),
            storage,
            network: Network::default(),
            volumes: Volumes::new(config.volumes_dir()),
            config,
        })
    }
}

async fn create_placeholder_drive(config: &Config) -> Result<()> {
    let path = config.placeholder_drive();
    match path.exists() {
        true => Ok(()),
        false => run("truncate", &["-s", PLACEHOLDER_SIZE, &path.display().to_string()]).await,
    }
}
