use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use super::services::Services;
use crate::network::slot::MAX_SLOT;
use crate::storage::backend::Storage;

const DEFAULT_GRPC_PORT: u16 = 5008;
const DEFAULT_NODE_ID: &str = "local";
const DEFAULT_ENVD_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_SANDBOXES: i64 = 200;
const DEFAULT_MAX_STARTING: usize = 3;
const DEFAULT_DRAIN_SLEEP_MS: u64 = 15_000;
const DEFAULT_BUILD_CACHE_MAX_USAGE: f64 = 85.0;
const DEFAULT_ORCHESTRATOR_IP: &str = "192.0.2.1";
const DEFAULT_HYPERLOOP_PORT: u16 = 5010;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageConfig {
    Local {
        root: PathBuf,
    },
    S3 {
        bucket: String,
        endpoint: Option<String>,
        key: String,
        secret: String,
        region: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeConfig {
    pub grpc_port: u16,
    pub node_id: String,
    pub firecracker: PathBuf,
    pub kernel: PathBuf,
    pub storage: StorageConfig,
    pub work_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub template_cache_dir: PathBuf,
    pub slots: RangeInclusive<u32>,
    pub local_expiry: bool,
    pub envd_timeout: Duration,
    pub labels: Vec<String>,
    pub max_sandboxes: i64,
    pub max_starting: usize,
    pub drain_sleep: Duration,
    pub build_cache_max_usage: f64,
    pub prefetch_from_trace: bool,
    pub use_memfd: bool,
    pub services: Services,
    pub orchestrator_ip: String,
    pub hyperloop_port: u16,
    pub logs_collector: String,
}

impl NodeConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(&std::env::vars().collect())
    }

    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        let var = |name: &str| vars.get(name).cloned();
        let required = |name: &str| var(name).ok_or_else(|| format!("{name} is not set"));
        let grpc_port = var("GRPC_PORT")
            .map(|port| port.parse().map_err(|_| format!("GRPC_PORT is not a port: {port}")))
            .transpose()?
            .unwrap_or(DEFAULT_GRPC_PORT);
        let cache_dir: PathBuf = var("E2B_CACHE_DIR")
            .unwrap_or_else(|| "/var/lib/e2b/cache".to_owned())
            .into();
        Ok(Self {
            grpc_port,
            node_id: var("NODE_ID").unwrap_or_else(|| DEFAULT_NODE_ID.to_owned()),
            firecracker: required("E2B_FIRECRACKER")?.into(),
            kernel: required("E2B_KERNEL")?.into(),
            storage: storage_config(vars)?,
            work_dir: var("E2B_WORK_DIR")
                .unwrap_or_else(|| "/var/lib/e2b/sandboxes".to_owned())
                .into(),
            template_cache_dir: var("E2B_TEMPLATE_CACHE_DIR").map_or_else(|| cache_dir.join("template"), PathBuf::from),
            cache_dir,
            slots: 1..=MAX_SLOT,
            local_expiry: var("E2B_LOCAL_EXPIRY").is_some_and(|value| value == "1" || value == "true"),
            envd_timeout: var("ENVD_TIMEOUT_MS")
                .map(|ms| ms.parse().map_err(|_| format!("ENVD_TIMEOUT_MS is not a number: {ms}")))
                .transpose()?
                .map_or(Duration::from_millis(DEFAULT_ENVD_TIMEOUT_MS), Duration::from_millis),
            labels: var("NODE_LABELS")
                .map(|labels| {
                    labels
                        .split(',')
                        .filter(|label| !label.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            max_sandboxes: number(vars, "MAX_SANDBOXES_PER_NODE")?.unwrap_or(DEFAULT_MAX_SANDBOXES),
            max_starting: number(vars, "MAX_STARTING_INSTANCES_PER_NODE")?.unwrap_or(DEFAULT_MAX_STARTING),
            drain_sleep: Duration::from_millis(
                number(vars, "SHUTDOWN_DRAIN_SLEEP_MS")?.unwrap_or(DEFAULT_DRAIN_SLEEP_MS),
            ),
            build_cache_max_usage: number(vars, "BUILD_CACHE_MAX_USAGE_PERCENTAGE")?
                .unwrap_or(DEFAULT_BUILD_CACHE_MAX_USAGE),
            prefetch_from_trace: prefetch_source(var("RESUME_PREFETCH_SOURCE").as_deref()),
            use_memfd: var("USE_MEMFD").is_none_or(|value| value != "0" && value != "false"),
            services: Services::parse(var("ORCHESTRATOR_SERVICES").as_deref()),
            orchestrator_ip: var("SANDBOX_ORCHESTRATOR_IP").unwrap_or_else(|| DEFAULT_ORCHESTRATOR_IP.to_owned()),
            hyperloop_port: number(vars, "SANDBOX_HYPERLOOP_PROXY_PORT")?.unwrap_or(DEFAULT_HYPERLOOP_PORT),
            logs_collector: var("LOGS_COLLECTOR_ADDRESS").unwrap_or_default(),
        })
    }

    pub fn open_storage(&self) -> Result<Storage, String> {
        match &self.storage {
            StorageConfig::Local { root } => Storage::local(root),
            StorageConfig::S3 {
                bucket,
                endpoint,
                key,
                secret,
                region,
            } => endpoint
                .iter()
                .fold(Storage::s3().bucket(bucket), |builder, endpoint| {
                    builder.endpoint(endpoint)
                })
                .credentials(key, secret)
                .region(region)
                .build(),
        }
    }
}

fn storage_config(vars: &HashMap<String, String>) -> Result<StorageConfig, String> {
    let var = |name: &str| vars.get(name).cloned();
    match (var("E2B_STORAGE_DIR"), var("E2B_S3_BUCKET")) {
        (Some(root), _) => Ok(StorageConfig::Local { root: root.into() }),
        (None, Some(bucket)) => Ok(StorageConfig::S3 {
            bucket,
            endpoint: var("E2B_S3_ENDPOINT"),
            key: var("E2B_S3_KEY").unwrap_or_default(),
            secret: var("E2B_S3_SECRET").unwrap_or_default(),
            region: var("E2B_S3_REGION").unwrap_or_else(|| "us-east-1".to_owned()),
        }),
        (None, None) => Err("set E2B_STORAGE_DIR or E2B_S3_BUCKET".to_owned()),
    }
}

fn prefetch_source(source: Option<&str>) -> bool {
    !matches!(source, Some("off" | "last-cycle"))
}

fn number<T: std::str::FromStr>(vars: &HashMap<String, String>, name: &str) -> Result<Option<T>, String> {
    vars.get(name)
        .map(|value| value.parse().map_err(|_| format!("{name} is not a number: {value}")))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_and_local_storage() {
        let config = NodeConfig::from_vars(&vars(&[
            ("E2B_FIRECRACKER", "/opt/fc"),
            ("E2B_KERNEL", "/opt/vmlinux"),
            ("E2B_STORAGE_DIR", "/srv/builds"),
        ]))
        .unwrap();

        assert_eq!(config.grpc_port, 5008);
        assert_eq!(config.node_id, "local");
        assert!(!config.local_expiry, "expiry belongs to the API unless asked for");
        assert_eq!(config.template_cache_dir, PathBuf::from("/var/lib/e2b/cache/template"));
        assert_eq!(config.envd_timeout, Duration::from_secs(10));
        assert_eq!((config.max_sandboxes, config.max_starting), (200, 3));
        assert_eq!(config.drain_sleep, Duration::from_secs(15));
        assert!(config.prefetch_from_trace);
        assert!(config.use_memfd);
        assert_eq!(
            (config.orchestrator_ip.as_str(), config.hyperloop_port),
            ("192.0.2.1", 5010)
        );
        assert!(!prefetch_source(Some("off")));
        assert!(prefetch_source(Some("unknown")));
        assert!(config.labels.is_empty());
        assert_eq!(
            config.storage,
            StorageConfig::Local {
                root: "/srv/builds".into()
            }
        );
    }

    #[test]
    fn missing_or_bad_values_are_errors() {
        assert_eq!(
            NodeConfig::from_vars(&vars(&[("E2B_KERNEL", "/k"), ("E2B_STORAGE_DIR", "/s")])).unwrap_err(),
            "E2B_FIRECRACKER is not set"
        );
        assert_eq!(
            NodeConfig::from_vars(&vars(&[("E2B_FIRECRACKER", "/f"), ("E2B_KERNEL", "/k")])).unwrap_err(),
            "set E2B_STORAGE_DIR or E2B_S3_BUCKET"
        );
        assert_eq!(
            NodeConfig::from_vars(&vars(&[("GRPC_PORT", "x")])).unwrap_err(),
            "GRPC_PORT is not a port: x"
        );
    }
}
