use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sandbox::{MountRequest, SandboxInfo};
use crate::volume::Volume;

const DEFAULT_TIMEOUT_SECONDS: u64 = 15;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSandbox {
    #[serde(rename = "templateID")]
    pub template_id: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub auto_pause: bool,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    #[serde(default)]
    pub volume_mounts: Vec<VolumeMountModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMountModel {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct Timeout {
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

#[derive(Debug, Default, Deserialize)]
pub struct Refresh {
    #[serde(default)]
    pub duration: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct NewSnapshot {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NewVolume {
    pub name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxModel {
    #[serde(rename = "templateID")]
    pub template_id: String,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    #[serde(rename = "clientID")]
    pub client_id: String,
    pub envd_version: String,
    pub envd_access_token: String,
    pub traffic_access_token: Option<String>,
    pub domain: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDetailModel {
    #[serde(rename = "templateID")]
    pub template_id: String,
    #[serde(rename = "sandboxID")]
    pub sandbox_id: String,
    #[serde(rename = "clientID")]
    pub client_id: String,
    pub started_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub cpu_count: u32,
    #[serde(rename = "memoryMB")]
    pub memory_mb: u32,
    #[serde(rename = "diskSizeMB")]
    pub disk_size_mb: u64,
    pub metadata: HashMap<String, String>,
    pub state: &'static str,
    pub envd_version: String,
    pub envd_access_token: String,
    pub domain: Option<String>,
    pub volume_mounts: Vec<VolumeMountModel>,
}

#[derive(Debug, Serialize)]
pub struct SnapshotModel {
    #[serde(rename = "snapshotID")]
    pub snapshot_id: String,
    pub names: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct VolumeModel {
    #[serde(rename = "volumeID")]
    pub volume_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

impl From<VolumeMountModel> for MountRequest {
    fn from(mount: VolumeMountModel) -> Self {
        Self {
            name: mount.name,
            path: mount.path,
        }
    }
}

impl From<&MountRequest> for VolumeMountModel {
    fn from(mount: &MountRequest) -> Self {
        Self {
            name: mount.name.clone(),
            path: mount.path.clone(),
        }
    }
}

impl SandboxModel {
    pub fn new(info: &SandboxInfo) -> Self {
        Self {
            template_id: info.template_id.clone(),
            sandbox_id: info.sandbox_id.clone(),
            client_id: "local".to_owned(),
            envd_version: info.envd_version.clone(),
            envd_access_token: info.envd_token.clone(),
            traffic_access_token: None,
            domain: Some(info.domain.clone()),
        }
    }
}

impl SandboxDetailModel {
    pub fn new(info: &SandboxInfo) -> Self {
        Self {
            template_id: info.template_id.clone(),
            sandbox_id: info.sandbox_id.clone(),
            client_id: "local".to_owned(),
            started_at: info.started_at,
            end_at: info.end_at,
            cpu_count: info.vcpus,
            memory_mb: info.memory_mib,
            disk_size_mb: info.disk_mb,
            metadata: info.metadata.clone(),
            state: if info.paused { "paused" } else { "running" },
            envd_version: info.envd_version.clone(),
            envd_access_token: info.envd_token.clone(),
            domain: Some(info.domain.clone()),
            volume_mounts: info.volume_mounts.iter().map(VolumeMountModel::from).collect(),
        }
    }
}

impl VolumeModel {
    pub fn new(volume: &Volume, with_token: bool) -> Self {
        Self {
            volume_id: volume.volume_id.clone(),
            name: volume.name.clone(),
            token: with_token.then(|| volume.volume_id.clone()),
        }
    }
}
