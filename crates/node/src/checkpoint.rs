use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub vm_state: PathBuf,
    pub memory_layers: Vec<PathBuf>,
    pub rootfs_layers: Vec<PathBuf>,
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
    pub layers: Vec<PathBuf>,
}

impl Checkpoint {
    pub fn dir(root: &Path, id: &str) -> PathBuf {
        root.join(id)
    }

    pub fn load(root: &Path, id: &str) -> Result<Self> {
        read_json(&Self::dir(root, id).join("checkpoint.json"))
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        write_json(&Self::dir(root, &self.id).join("checkpoint.json"), self)
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("json path without parent")?)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn list_json<T: DeserializeOwned>(dir: &Path) -> Vec<T> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| read_json(&path).ok())
        .collect()
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
