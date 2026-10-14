use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use overlaybd::config::{ImageConfig, LayerConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::Store;

const PREFIX: &str = "layers";
const HASH_CHUNK: usize = 4 * 1024 * 1024;

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

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn publish(&self, sealed: &Path) -> Result<Layer> {
        let layer = describe(sealed).await?;
        self.add_to_cache(sealed, &layer)?;
        let key = format!("{PREFIX}/{}", layer.digest);
        if !self.store.exists(&key).await? {
            self.store.upload(&key, &self.cached(&layer)).await?;
        }
        Ok(layer)
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
        match cached.exists() {
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

    fn add_to_cache(&self, sealed: &Path, layer: &Layer) -> Result<()> {
        let cached = self.cached(layer);
        if cached.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        std::fs::hard_link(sealed, &cached).with_context(|| format!("cache layer {}", layer.digest))
    }

    fn cached(&self, layer: &Layer) -> PathBuf {
        self.dir.join(format!("{}.commit", layer.digest))
    }
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
