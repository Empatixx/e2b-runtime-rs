pub mod memory;
pub mod shared_memory;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use overlaybd::config::{CacheConfig, GlobalConfig, ImageConfig, LayerConfig, LogConfig, UpperMode};
use uvm_ublk_daemon::{CreateOverlaybdRuntimeDeviceRequest, PoolConfig, UblkDaemonClient, UblkDaemonSpawnConfig};

use crate::config::Config;

pub struct Storage {
    client: Arc<UblkDaemonClient>,
    global_config: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub dev_id: u32,
    pub device: PathBuf,
}

impl Storage {
    pub async fn start(config: &Config) -> Result<Self> {
        let global_config = write_global_config(&config.data_dir)?;
        let client = UblkDaemonClient::new(UblkDaemonSpawnConfig {
            binary_path: &config.ublk_daemon,
            socket_path: config.data_dir.join("ublk.sock"),
            global_config: &global_config,
            resize_global_config: &global_config,
            app_config: None,
            log_file: Some(&config.data_dir.join("ublk-daemon.log")),
            metrics_listen_addr: "127.0.0.1:9102",
            pool_config: Some(&warm_devices()),
            p2p_publish_url: None,
            runtime_device_timeout: Duration::from_secs(30),
        })
        .await
        .context("start ublk daemon")?;
        Ok(Self { client, global_config })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn open_writable(&self, dir: &Path, layers: &[PathBuf]) -> Result<Disk> {
        let source = write_image_config(&dir.join("source.json"), layers)?;
        let runtime_dir = dir.join("runtime");
        let device = self
            .client
            .create_overlaybd_runtime_device(CreateOverlaybdRuntimeDeviceRequest {
                source_image_config: &source,
                global_config: &self.global_config,
                runtime_dir: &runtime_dir,
                read_only: false,
                runtime_upper_mode: UpperMode::LogStructured,
                requested_virtual_size: None,
                known_source_virtual_size: None,
                allow_shrink: false,
            })
            .await?;
        Ok(Disk {
            dev_id: device.dev_id,
            device: device.device_path,
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn open_read_only(&self, dir: &Path, layers: &[PathBuf]) -> Result<Disk> {
        let image = write_image_config(&dir.join("image.json"), layers)?;
        let (dev_id, device) = self.client.create_overlaybd(&image, &self.global_config).await?;
        Ok(Disk { dev_id, device })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn seal(&self, disk: &Disk, layer: &Path) -> Result<PathBuf> {
        flush_host_cache(&disk.device).await?;
        self.client.restack_snapshot(disk.dev_id, layer).await?;
        Ok(layer.to_path_buf())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn release_writable(&self, disk: &Disk) {
        self.client
            .release_overlaybd(disk.dev_id)
            .await
            .unwrap_or_else(|e| tracing::warn!(dev_id = disk.dev_id, error = %e, "ublk release failed"));
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn close(&self, disk: &Disk) {
        self.client
            .delete(disk.dev_id)
            .await
            .unwrap_or_else(|e| tracing::warn!(dev_id = disk.dev_id, error = %e, "ublk delete failed"));
    }
}

fn warm_devices() -> PoolConfig {
    PoolConfig {
        low_watermark: 4,
        high_watermark: 16,
        maintenance_enabled: true,
        startup_prewarm: true,
    }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn flush_host_cache(device: &Path) -> Result<()> {
    let device = device.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(&device)?.sync_all())
        .await?
        .context("flush host page cache of the block device")
}

pub async fn package_raw_file(source: &Path, layer: &Path) -> Result<()> {
    overlaybd::tools::package_raw_as_overlaybd(source, layer).await
}

fn write_image_config(path: &Path, layers: &[PathBuf]) -> Result<PathBuf> {
    let config = ImageConfig {
        lowers: layers.iter().map(|layer| local_layer(layer)).collect(),
        result_file: path.with_extension("result").display().to_string(),
        ..Default::default()
    };
    std::fs::create_dir_all(path.parent().context("image config without parent")?)?;
    std::fs::write(path, serde_json::to_vec_pretty(&config)?)?;
    Ok(path.to_path_buf())
}

fn local_layer(path: &Path) -> LayerConfig {
    LayerConfig {
        file: path.display().to_string(),
        ..Default::default()
    }
}

fn write_global_config(data_dir: &Path) -> Result<PathBuf> {
    let cache = data_dir.join("cache");
    let logs = data_dir.join("logs");
    std::fs::create_dir_all(&cache)?;
    std::fs::create_dir_all(&logs)?;
    let config = GlobalConfig {
        registry_cache_dir: cache.join("registry").display().to_string(),
        credential_file_path: data_dir.join("cred.json").display().to_string(),
        log_path: logs.join("overlaybd.log").display().to_string(),
        audit_path: logs.join("overlaybd-audit.log").display().to_string(),
        enable_audit: false,
        cache_config: CacheConfig {
            cache_dir: cache.join("blocks").display().to_string(),
            ..Default::default()
        },
        log_config: LogConfig {
            log_path: logs.join("overlaybd.log").display().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let path = data_dir.join("overlaybd.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    Ok(path)
}
