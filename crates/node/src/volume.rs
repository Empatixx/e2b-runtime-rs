use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::checkpoint::{list_json, new_id, read_json, write_json};
use crate::network::run;
use crate::storage::package_raw_file;

const DEFAULT_SIZE_MB: u64 = 4096;
const GUEST_USER_OWNER: &str = "root_owner=1000:1000";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub volume_id: String,
    pub name: String,
    pub size_mb: u64,
    pub layers: Vec<PathBuf>,
    pub attached_to: Option<String>,
}

pub struct Volumes {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl Volumes {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            lock: Mutex::new(()),
        }
    }

    pub fn list(&self) -> Vec<Volume> {
        list_json(&self.dir)
    }

    pub fn get(&self, id_or_name: &str) -> Option<Volume> {
        self.list()
            .into_iter()
            .find(|volume| volume.volume_id == id_or_name || volume.name == id_or_name)
    }

    pub fn layer_path(&self, volume_id: &str, generation: usize) -> PathBuf {
        self.dir.join(volume_id).join(format!("layer-{generation}.commit"))
    }

    pub fn save(&self, volume: &Volume) -> Result<()> {
        write_json(&self.record(&volume.volume_id), volume)
    }

    pub async fn create(&self, name: &str) -> Result<Volume> {
        let _guard = self.lock.lock().await;
        if self.get(name).is_some() {
            bail!("volume {name} already exists");
        }
        let volume_id = new_id("vol");
        let base = self.layer_path(&volume_id, 0);
        format_empty_ext4(&self.dir.join(&volume_id), DEFAULT_SIZE_MB, &base).await?;
        let volume = Volume {
            volume_id,
            name: name.to_owned(),
            size_mb: DEFAULT_SIZE_MB,
            layers: vec![base],
            attached_to: None,
        };
        self.save(&volume)?;
        Ok(volume)
    }

    pub async fn attach(&self, id_or_name: &str, sandbox_id: &str) -> Result<Volume> {
        let _guard = self.lock.lock().await;
        let Some(mut volume) = self.get(id_or_name) else {
            bail!("volume {id_or_name} not found");
        };
        if let Some(owner) = volume.attached_to.as_ref().filter(|owner| *owner != sandbox_id) {
            bail!("volume {} is mounted by sandbox {owner}", volume.name);
        }
        volume.attached_to = Some(sandbox_id.to_owned());
        self.save(&volume)?;
        Ok(volume)
    }

    pub async fn commit(&self, volume_id: &str, layers: Vec<PathBuf>, detach: bool) -> Result<()> {
        let _guard = self.lock.lock().await;
        let mut volume: Volume = read_json(&self.record(volume_id))?;
        volume.layers = layers;
        if detach {
            volume.attached_to = None;
        }
        self.save(&volume)
    }

    pub async fn delete(&self, id_or_name: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let Some(volume) = self.get(id_or_name) else {
            return Ok(false);
        };
        if let Some(owner) = volume.attached_to {
            bail!("volume {} is mounted by sandbox {owner}", volume.name);
        }
        std::fs::remove_file(self.record(&volume.volume_id))?;
        let _ = std::fs::remove_dir_all(self.dir.join(&volume.volume_id));
        Ok(true)
    }

    fn record(&self, volume_id: &str) -> PathBuf {
        self.dir.join(format!("{volume_id}.json"))
    }
}

async fn format_empty_ext4(dir: &Path, size_mb: u64, layer: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let raw = dir.join("empty.ext4");
    let raw_path = raw.display().to_string();
    run("truncate", &["-s", &format!("{size_mb}M"), &raw_path]).await?;
    run("mkfs.ext4", &["-q", "-F", "-E", GUEST_USER_OWNER, &raw_path]).await?;
    package_raw_file(&raw, layer).await?;
    std::fs::remove_file(raw)?;
    Ok(())
}
