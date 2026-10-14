use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layers::Layer;
use crate::store::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub memory_layers: Vec<Layer>,
    pub rootfs_layers: Vec<Layer>,
    #[serde(default)]
    pub shared_memory_layers: usize,
    #[serde(default)]
    pub shared_rootfs_layers: usize,
    pub drives: Vec<DriveState>,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub disk_mb: u64,
    pub envd_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveState {
    pub slot: usize,
    pub mount_path: String,
    pub volume: Option<String>,
    pub layers: Vec<Layer>,
}

pub struct Checkpoints {
    store: Arc<Store>,
    dir: PathBuf,
}

impl Checkpoint {
    pub fn into_template(self) -> Self {
        let drives = self
            .drives
            .into_iter()
            .map(|drive| DriveState { volume: None, ..drive })
            .collect();
        Self {
            shared_memory_layers: self.memory_layers.len(),
            shared_rootfs_layers: self.rootfs_layers.len(),
            drives,
            ..self
        }
    }
}

impl Checkpoints {
    pub fn new(store: Arc<Store>, dir: PathBuf) -> Self {
        Self { store, dir }
    }

    pub fn new_dir(&self) -> PathBuf {
        self.dir.join(new_id("ckpt"))
    }

    pub async fn save(&self, checkpoint: &Checkpoint) -> Result<()> {
        let vm_state = self.local_vm_state(&checkpoint.id);
        self.store.upload(&vm_state_key(&checkpoint.id), &vm_state).await?;
        self.store.put(&record_key(&checkpoint.id), checkpoint).await
    }

    pub async fn load(&self, id: &str) -> Result<Checkpoint> {
        let found = self.store.get(&record_key(id)).await?;
        Ok(found.with_context(|| format!("checkpoint {id} not found"))?.value)
    }

    pub async fn delete(&self, id: &str) {
        let removed = async {
            self.store.delete(&record_key(id)).await?;
            self.store.delete(&vm_state_key(id)).await
        }
        .await;
        removed
            .unwrap_or_else(|e| tracing::warn!(checkpoint = id, error = format!("{e:#}"), "delete checkpoint failed"));
        let _ = std::fs::remove_dir_all(self.dir.join(id));
    }

    pub async fn vm_state(&self, checkpoint: &Checkpoint) -> Result<PathBuf> {
        let local = self.local_vm_state(&checkpoint.id);
        if !local.exists() {
            self.store.download(&vm_state_key(&checkpoint.id), &local).await?;
        }
        Ok(local)
    }

    fn local_vm_state(&self, id: &str) -> PathBuf {
        self.dir.join(id).join("vmstate")
    }
}

pub fn checkpoint_id(dir: &Path) -> Result<String> {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .context("checkpoint directory without a name")
}

fn record_key(id: &str) -> String {
    format!("checkpoints/{id}.json")
}

fn vm_state_key(id: &str) -> String {
    format!("checkpoints/{id}/vmstate")
}

pub fn new_id(prefix: &str) -> String {
    use rand::Rng;
    let suffix: String = rand::thread_rng()
        .sample_iter(rand::distributions::Alphanumeric)
        .take(20)
        .map(|c| (c as char).to_ascii_lowercase())
        .collect();
    format!("{prefix}{suffix}")
}
