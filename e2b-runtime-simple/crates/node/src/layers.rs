pub mod cache;
pub mod merge;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use overlaybd::config::{ImageConfig, LayerConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::Store;

const PREFIX: &str = "layers";
const HASH_CHUNK: usize = 4 * 1024 * 1024;
const MAX_OWN_LAYERS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    pub digest: String,
    pub size: u64,
}

pub struct Layers {
    store: Arc<Store>,
    dir: PathBuf,
}

impl Layers {
    pub fn new(store: Arc<Store>, dir: PathBuf) -> Self {
        Self { store, dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn publish(&self, sealed: &Path) -> Result<Layer> {
        let layer = describe(sealed).await?;
        self.move_into_cache(sealed, &layer)?;
        let key = object_key(&layer);
        if !self.store.exists(&key).await? {
            self.store.upload(&key, &self.cached(&layer)).await?;
        }
        Ok(layer)
    }

    pub async fn local_copy(&self, layer: &Layer) -> Result<PathBuf> {
        let cached = self.cached(layer);
        if !cached.exists() {
            self.store.download(&object_key(layer), &cached).await?;
        }
        Ok(cached)
    }

    pub async fn compact(&self, stack: &mut Vec<Layer>, shared: usize, scratch: &Path) -> Result<()> {
        if stack.len().saturating_sub(shared) <= own_layer_budget(shared) {
            return Ok(());
        }
        let mut sources = Vec::new();
        for layer in &stack[shared..] {
            sources.push(self.local_copy(layer).await?);
        }
        merge::merge_layers(&sources, scratch).await?;
        let merged = self.publish(scratch).await?;
        tracing::info!(
            layers = stack.len() - shared,
            digest = merged.digest,
            "merged own layers"
        );
        stack.truncate(shared);
        stack.push(merged);
        Ok(())
    }

    pub fn write_image_config(&self, path: &Path, layers: &[Layer]) -> Result<PathBuf> {
        let config = ImageConfig {
            repo_blob_url: self.store.url(PREFIX),
            lowers: layers.iter().map(|layer| self.lower(layer)).collect(),
            result_file: path.with_extension("result").display().to_string(),
            ..Default::default()
        };
        std::fs::create_dir_all(path.parent().context("image config without parent")?)?;
        std::fs::write(path, serde_json::to_vec_pretty(&config)?)?;
        Ok(path.to_path_buf())
    }

    fn lower(&self, layer: &Layer) -> LayerConfig {
        let cached = self.cached(layer);
        match mark_used(&cached) {
            true => LayerConfig {
                file: cached.display().to_string(),
                ..Default::default()
            },
            false => LayerConfig {
                digest: layer.digest.clone(),
                size: layer.size,
                ..Default::default()
            },
        }
    }

    fn move_into_cache(&self, sealed: &Path, layer: &Layer) -> Result<()> {
        let cached = self.cached(layer);
        if cached.exists() {
            return Ok(std::fs::remove_file(sealed)?);
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::rename(sealed, &cached).with_context(|| format!("cache layer {}", layer.digest))
    }

    fn cached(&self, layer: &Layer) -> PathBuf {
        self.dir.join(format!("{}.commit", layer.digest))
    }
}

pub fn own_layer_budget(shared: usize) -> usize {
    MAX_OWN_LAYERS.saturating_sub(shared / 4).max(1)
}

fn object_key(layer: &Layer) -> String {
    format!("{PREFIX}/{}", layer.digest)
}

fn mark_used(cached: &Path) -> bool {
    std::fs::File::options()
        .write(true)
        .open(cached)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .is_ok()
}

async fn describe(path: &Path) -> Result<Layer> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path).with_context(|| format!("open layer {}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; HASH_CHUNK];
        let mut size = 0u64;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        Ok(Layer {
            digest: format!("sha256:{}", hex::encode(hasher.finalize())),
            size,
        })
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_shrinks_with_the_shared_prefix_like_agentenv() {
        assert_eq!(own_layer_budget(0), 32);
        assert_eq!(own_layer_budget(3), 32);
        assert_eq!(own_layer_budget(32), 24);
        assert_eq!(own_layer_budget(200), 1);
    }
}
