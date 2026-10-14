use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::checkpoint::new_id;
use crate::layers::Layer;
use crate::network::run;
use crate::storage::{Storage, package_raw_file};
use crate::store::{Store, Versioned};

const PREFIX: &str = "volumes/";
const DEFAULT_SIZE_MB: u64 = 4096;
const GUEST_USER_OWNER: &str = "root_owner=1000:1000";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub volume_id: String,
    pub name: String,
    pub size_mb: u64,
    pub layers: Vec<Layer>,
    pub attached_to: Option<String>,
}

pub struct Volumes {
    store: Arc<Store>,
    storage: Arc<Storage>,
    dir: PathBuf,
}

impl Volumes {
    pub fn new(store: Arc<Store>, storage: Arc<Storage>, dir: PathBuf) -> Self {
        Self { store, storage, dir }
    }

    pub async fn list(&self) -> Result<Vec<Volume>> {
        self.store.list(PREFIX).await
    }

    pub async fn get(&self, id_or_name: &str) -> Result<Option<Volume>> {
        if let Some(found) = self.store.get::<Volume>(&record_key(id_or_name)).await? {
            return Ok(Some(found.value));
        }
        Ok(self.list().await?.into_iter().find(|volume| volume.name == id_or_name))
    }

    pub fn sealed_layer_path(&self, volume_id: &str) -> PathBuf {
        self.dir
            .join(volume_id)
            .join(new_id("sealed-"))
            .with_extension("commit")
    }

    pub async fn create(&self, name: &str) -> Result<Volume> {
        if self.get(name).await?.is_some() {
            bail!("volume {name} already exists");
        }
        let volume_id = new_id("vol");
        let base = self.empty_ext4_layer(&volume_id).await?;
        let volume = Volume {
            volume_id,
            name: name.to_owned(),
            size_mb: DEFAULT_SIZE_MB,
            layers: vec![base],
            attached_to: None,
        };
        self.store.create(&record_key(&volume.volume_id), &volume).await?;
        Ok(volume)
    }

    pub async fn attach(&self, id_or_name: &str, sandbox_id: &str) -> Result<Volume> {
        let volume = self
            .get(id_or_name)
            .await?
            .with_context(|| format!("volume {id_or_name} not found"))?;
        let current = self.versioned(&volume.volume_id).await?;
        if let Some(owner) = current.value.attached_to.as_ref().filter(|owner| *owner != sandbox_id) {
            bail!("volume {} is mounted by sandbox {owner}", volume.name);
        }
        let attached = Volume {
            attached_to: Some(sandbox_id.to_owned()),
            ..current.value.clone()
        };
        match self
            .store
            .replace(&record_key(&volume.volume_id), &current, &attached)
            .await?
        {
            true => Ok(attached),
            false => bail!("volume {} was mounted by another sandbox at the same time", volume.name),
        }
    }

    pub async fn commit(&self, volume_id: &str, layers: Vec<Layer>, detach: bool) -> Result<()> {
        let current = self.versioned(volume_id).await?.value;
        let committed = Volume {
            layers,
            attached_to: if detach { None } else { current.attached_to.clone() },
            ..current
        };
        self.store.put(&record_key(volume_id), &committed).await
    }

    pub async fn delete(&self, id_or_name: &str) -> Result<bool> {
        let Some(volume) = self.get(id_or_name).await? else {
            return Ok(false);
        };
        if let Some(owner) = volume.attached_to {
            bail!("volume {} is mounted by sandbox {owner}", volume.name);
        }
        self.store.delete(&record_key(&volume.volume_id)).await?;
        Ok(true)
    }

    async fn versioned(&self, volume_id: &str) -> Result<Versioned<Volume>> {
        let found = self.store.get(&record_key(volume_id)).await?;
        found.with_context(|| format!("volume {volume_id} not found"))
    }

    async fn empty_ext4_layer(&self, volume_id: &str) -> Result<Layer> {
        let dir = self.dir.join(volume_id);
        std::fs::create_dir_all(&dir)?;
        let raw = dir.join("empty.ext4");
        let sealed = dir.join("base.commit");
        format_empty_ext4(&raw, DEFAULT_SIZE_MB).await?;
        package_raw_file(&raw, &sealed).await?;
        std::fs::remove_file(raw)?;
        self.storage.publish(&sealed).await
    }
}

fn record_key(volume_id: &str) -> String {
    format!("{PREFIX}{volume_id}.json")
}

async fn format_empty_ext4(raw: &Path, size_mb: u64) -> Result<()> {
    let raw = raw.display().to_string();
    run("truncate", &["-s", &format!("{size_mb}M"), &raw]).await?;
    run("mkfs.ext4", &["-q", "-F", "-E", GUEST_USER_OWNER, &raw]).await
}
