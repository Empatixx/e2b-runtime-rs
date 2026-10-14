pub mod memory;
pub mod shared_memory;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use overlaybd::config::{CacheConfig, GlobalConfig, LogConfig, OssConfig, UpperMode};
use uvm_ublk_daemon::{CreateOverlaybdRuntimeDeviceRequest, PoolConfig, UblkDaemonClient, UblkDaemonSpawnConfig};

use crate::config::{Config, S3Config};
use crate::layers::{Layer, Layers};

const REMOTE_IO_WORKERS: usize = 4;

pub struct Storage {
    client: Arc<UblkDaemonClient>,
    global_config: PathBuf,
    layers: Arc<Layers>,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub dev_id: u32,
    pub device: PathBuf,
}

impl Storage {
    pub async fn start(config: &Config, layers: Arc<Layers>) -> Result<Self> {
        let global_config = write_global_config(&config.data_dir, &config.s3)?;
        let client = UblkDaemonClient::new(UblkDaemonSpawnConfig {
            binary_path: &config.ublk_daemon,
            socket_path: config.data_dir.join("ublk.sock"),
            global_config: &global_config,
            resize_global_config: &global_config,
            app_config: None,
            log_file: Some(&config.data_dir.join("ublk-daemon.log")),
            metrics_listen_addr: "127.0.0.1:0",
            pool_config: Some(&warm_devices()),
            p2p_publish_url: None,
            runtime_device_timeout: Duration::from_secs(30),
        })
        .await
        .context("start ublk daemon")?;
        Ok(Self {
            client,
            global_config,
            layers,
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn open_writable(&self, dir: &Path, layers: &[Layer]) -> Result<Disk> {
        let source = self.layers.write_image_config(&dir.join("source.json"), layers)?;
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
    pub async fn open_read_only(&self, dir: &Path, layers: &[Layer]) -> Result<Disk> {
        let image = self.layers.write_image_config(&dir.join("image.json"), layers)?;
        let (dev_id, device) = self.client.create_overlaybd(&image, &self.global_config).await?;
        Ok(Disk { dev_id, device })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn seal(&self, disk: &Disk, sealed: &Path) -> Result<Layer> {
        std::fs::create_dir_all(sealed.parent().context("sealed layer without parent")?)?;
        flush_host_cache(&disk.device).await?;
        self.client.restack_snapshot(disk.dev_id, sealed).await?;
        self.layers.publish(sealed).await
    }

    pub async fn publish(&self, sealed: &Path) -> Result<Layer> {
        self.layers.publish(sealed).await
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

fn write_global_config(data_dir: &Path, s3: &S3Config) -> Result<PathBuf> {
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
        oss_config: oss_config(s3),
        remote_io_workers: REMOTE_IO_WORKERS,
        ..Default::default()
    };
    let path = data_dir.join("overlaybd.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    Ok(path)
}

fn oss_config(s3: &S3Config) -> OssConfig {
    OssConfig {
        enable: true,
        access_key_id: s3.access_key.clone(),
        secret_access_key: s3.secret_key.clone(),
        default_region: s3.region.clone(),
        default_endpoint: s3.endpoint.clone(),
        default_addressing_style: "path".to_owned(),
        ..Default::default()
    }
}
