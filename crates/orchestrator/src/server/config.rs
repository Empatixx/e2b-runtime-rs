use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::PathBuf;

pub mod binaries;
pub mod nfs_proxy;
pub mod runtime;
mod storage;
pub mod volume_mounts;

use super::services::Services;
use super::upload::UploadHandler;
use crate::builder::config::BuilderConfig;
use crate::clickhouse::endpoint::Endpoint;
use crate::exposure::Transport;
use crate::featureflags::feature_flags_builder::{
    SetApiKey, SetDeploymentEnvironment, SetServiceName, SetServiceVersion,
};
use crate::featureflags::{FeatureFlags, FeatureFlagsBuilder, FlagContext, LAUNCH_DARKLY_API_KEY};
use crate::go_strconv::env_bool;
use crate::network::slot::MAX_SLOT;
use crate::routing::redis::RedisSettings;
use crate::storage::backend::Storage;
use crate::storage::local_upload::UploadSigner;
use crate::storage::spec::{Provider, StorageSpec};
use crate::telemetry::identity::service_name;
use binaries::Binaries;
use nfs_proxy::NfsProxySettings;
use runtime::RuntimeFlags;
use storage::StorageEnv;
use volume_mounts::VolumeMounts;

const DEFAULT_GRPC_PORT: u16 = 5008;
const DEFAULT_NODE_ID: &str = "local";
const DEFAULT_ORCHESTRATOR_IP: &str = "192.0.2.1";
const DEFAULT_HYPERLOOP_PORT: u16 = 5010;
const DEFAULT_PROXY_PORT: u16 = 5007;

#[derive(Clone)]
pub struct BuildStorage {
    pub storage: Storage,
    pub uploads: Option<UploadHandler>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeConfig {
    pub grpc_port: u16,
    pub node_id: String,
    pub firecracker: PathBuf,
    pub kernel: PathBuf,
    pub binaries: Binaries,
    pub network_version: u8,
    pub template_storage: StorageSpec,
    pub build_cache_storage: Option<StorageSpec>,
    pub local_upload_base_url: String,
    pub builder: BuilderConfig,
    pub work_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub template_cache_dir: PathBuf,
    pub slots: RangeInclusive<u32>,
    pub local_expiry: bool,
    pub labels: Vec<String>,
    pub deployment_environment: String,
    pub instance_group_name: String,
    pub feature_flags: FeatureFlags,
    pub runtime_flags: RuntimeFlags,
    pub local: bool,
    pub development: bool,
    pub force_stop: bool,
    pub services: Services,
    pub orchestrator_ip: String,
    pub hyperloop_port: u16,
    pub proxy_port: u16,
    pub logs_collector: String,
    pub node_ip: String,
    pub redis: Option<RedisSettings>,
    pub clickhouse: Option<String>,
    pub clickhouse_additional: Vec<String>,
    pub volume_mounts: VolumeMounts,
    pub nfs_proxy: NfsProxySettings,
    pub disable_startup_reclaim: bool,
    pub rootfs_exposure: Transport,
}

impl NodeConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(&std::env::vars().collect())
    }

    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        let var = |name: &str| vars.get(name).cloned();
        let grpc_port = var("GRPC_PORT")
            .map(|port| port.parse().map_err(|_| format!("GRPC_PORT is not a port: {port}")))
            .transpose()?
            .unwrap_or(DEFAULT_GRPC_PORT);
        let clickhouse = Endpoint::parse_list(
            var("CLICKHOUSE_CONNECTION_STRING").as_deref(),
            var("CLICKHOUSE_CONNECTION_STRINGS").as_deref(),
        );
        let services = Services::parse(var("ORCHESTRATOR_SERVICES").as_deref());
        let storage = StorageEnv::from_vars(vars);
        let template_storage = storage
            .clone()
            .and_then(|storage| storage.template())
            .map_err(|e| format!("failed to resolve template storage: {e}"))?;
        let build_cache_storage = services
            .runs_template_manager()
            .then(|| storage.and_then(|storage| storage.build_cache()))
            .transpose()
            .map_err(|e| format!("failed to setup build storage: resolve build cache storage: {e}"))?;
        let firecracker: PathBuf = var("E2B_FIRECRACKER").unwrap_or_default().into();
        let kernel: PathBuf = var("E2B_KERNEL").unwrap_or_default().into();
        let builder = BuilderConfig::from_vars(vars);
        Ok(Self {
            grpc_port,
            node_id: var("NODE_ID").unwrap_or_else(|| DEFAULT_NODE_ID.to_owned()),
            binaries: Binaries::from_vars(vars, &builder.target_arch, &firecracker, &kernel),
            network_version: network_version(vars)?,
            firecracker,
            kernel,
            template_storage,
            build_cache_storage,
            local_upload_base_url: var("LOCAL_UPLOAD_BASE_URL").unwrap_or_default(),
            work_dir: builder.sandbox_cache_dir.clone(),
            cache_dir: builder.default_cache_dir.clone(),
            template_cache_dir: builder.template_cache_dir.clone(),
            builder,
            slots: 1..=MAX_SLOT,
            local_expiry: var("E2B_LOCAL_EXPIRY").is_some_and(|value| value == "1" || value == "true"),
            labels: var("NODE_LABELS")
                .map(|labels| {
                    labels
                        .split(',')
                        .filter(|label| !label.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            deployment_environment: var("DEPLOYMENT_ENVIRONMENT").unwrap_or_default(),
            instance_group_name: var("INSTANCE_GROUP_NAME").unwrap_or_default(),
            feature_flags: FeatureFlags::offline(),
            runtime_flags: RuntimeFlags::new(FeatureFlags::offline(), services.clone()),
            local: var("ENVIRONMENT").is_some_and(|environment| environment == "local"),
            development: var("ENVIRONMENT").is_some_and(|environment| environment == "local" || environment == "dev"),
            force_stop: env_bool(var("FORCE_STOP").as_deref(), "ForceStop")?,
            services,
            orchestrator_ip: var("SANDBOX_ORCHESTRATOR_IP").unwrap_or_else(|| DEFAULT_ORCHESTRATOR_IP.to_owned()),
            hyperloop_port: number(vars, "SANDBOX_HYPERLOOP_PROXY_PORT")?.unwrap_or(DEFAULT_HYPERLOOP_PORT),
            proxy_port: number(vars, "PROXY_PORT")?.unwrap_or(DEFAULT_PROXY_PORT),
            logs_collector: var("LOGS_COLLECTOR_ADDRESS").unwrap_or_default(),
            node_ip: var("NODE_IP").unwrap_or_else(|| "localhost".to_owned()),
            redis: RedisSettings::from_vars(vars)?,
            clickhouse: clickhouse.0,
            clickhouse_additional: clickhouse.1,
            volume_mounts: VolumeMounts::from_vars(vars)?,
            nfs_proxy: NfsProxySettings::from_vars(vars)?,
            disable_startup_reclaim: env_bool(var("DISABLE_STARTUP_RECLAIM").as_deref(), "DisableStartupReclaim")?,
            rootfs_exposure: Transport::parse(var("ROOTFS_EXPOSURE").as_deref())?,
        })
    }

    pub fn feature_flags_builder(
        &self,
        commit: &str,
    ) -> FeatureFlagsBuilder<SetServiceVersion<SetServiceName<SetDeploymentEnvironment<SetApiKey>>>> {
        FeatureFlags::builder()
            .api_key(std::env::var(LAUNCH_DARKLY_API_KEY).unwrap_or_default())
            .deployment_environment(self.deployment_environment.as_str())
            .service_name(service_name(&self.services))
            .service_version(crate::version())
            .context_provider(FlagContext::orchestrator(&self.node_id, commit))
            .context_provider(FlagContext::instance_group(self.instance_group_name.trim()))
    }

    pub fn with_feature_flags(mut self, feature_flags: FeatureFlags) -> Self {
        self.builder.feature_flags = feature_flags.clone();
        self.runtime_flags = RuntimeFlags::new(feature_flags.clone(), self.services.clone());
        self.feature_flags = feature_flags;
        self
    }

    pub fn open_template_storage(&self) -> Result<Storage, String> {
        Storage::open(&self.template_storage, None)
            .map_err(|e| format!("failed to create template storage provider: {e}"))
    }

    pub fn node_address(&self) -> Option<String> {
        match (self.node_ip.as_str(), self.node_ip.contains(':')) {
            ("localhost", _) => None,
            (ip, true) => Some(format!("[{ip}]:{}", self.grpc_port)),
            (ip, false) => Some(format!("{ip}:{}", self.grpc_port)),
        }
    }

    pub fn open_build_storage(&self) -> Result<Option<BuildStorage>, String> {
        self.build_cache_storage
            .as_ref()
            .map(|spec| self.build_storage(spec))
            .transpose()
            .map_err(|e| format!("failed to setup build storage: {e}"))
    }

    fn build_storage(&self, spec: &StorageSpec) -> Result<BuildStorage, String> {
        let uploads = (spec.provider == Provider::Local).then(|| {
            let base_url = self.upload_base_url();
            tracing::info!(upload_base_url = %base_url, base_path = %spec.base_path, "Local upload endpoint enabled for filesystem storage");
            UploadHandler {
                base_path: spec.base_path.clone().into(),
                signer: UploadSigner::generate(&base_url),
            }
        });
        Storage::open(spec, uploads.as_ref().map(|uploads| uploads.signer.clone()))
            .map(|storage| BuildStorage { storage, uploads })
            .map_err(|e| format!("create build cache storage provider: {e}"))
    }

    fn upload_base_url(&self) -> String {
        match self.local_upload_base_url.as_str() {
            "" => format!("http://localhost:{}", self.grpc_port),
            url => url.to_owned(),
        }
    }
}

fn network_version(vars: &HashMap<String, String>) -> Result<u8, String> {
    match number::<i64>(vars, "NETWORK_VERSION")?.unwrap_or(1) {
        1 => Ok(1),
        2 => Ok(2),
        other => Err(format!("NETWORK_VERSION={other} unsupported (must be 1 or 2)")),
    }
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
    fn the_network_version_defaults_to_one_and_accepts_only_one_or_two() {
        assert_eq!(network_version(&vars(&[])), Ok(1));
        assert_eq!(network_version(&vars(&[("NETWORK_VERSION", "2")])), Ok(2));
        assert_eq!(
            network_version(&vars(&[("NETWORK_VERSION", "3")])),
            Err("NETWORK_VERSION=3 unsupported (must be 1 or 2)".to_owned())
        );
    }

    #[test]
    fn the_binaries_directories_come_from_the_node_vars() {
        let config = NodeConfig::from_vars(&vars(&[
            ("FIRECRACKER_VERSIONS_DIR", "/opt/fc"),
            ("STORAGE_PROVIDER", "Local"),
            ("LOCAL_TEMPLATE_STORAGE_BASE_PATH", "/srv/builds"),
        ]))
        .unwrap();

        assert_eq!(
            config.binaries.firecracker_for("v1").parent().and_then(|p| p.parent()),
            Some(std::path::Path::new("/opt/fc"))
        );
    }

    #[test]
    fn defaults_and_local_storage() {
        let config = NodeConfig::from_vars(&vars(&[
            ("E2B_FIRECRACKER", "/opt/fc"),
            ("E2B_KERNEL", "/opt/vmlinux"),
            ("STORAGE_PROVIDER", "Local"),
            ("LOCAL_TEMPLATE_STORAGE_BASE_PATH", "/srv/builds"),
        ]))
        .unwrap();

        assert_eq!(config.grpc_port, 5008);
        assert_eq!(config.node_id, "local");
        assert!(!config.local_expiry, "expiry belongs to the API unless asked for");
        assert_eq!(
            (&config.work_dir, &config.cache_dir, &config.template_cache_dir),
            (
                &PathBuf::from("/orchestrator/sandbox"),
                &PathBuf::from("/orchestrator/build"),
                &PathBuf::from("/orchestrator/template")
            )
        );
        assert!(!config.local && !config.force_stop);
        assert_eq!(
            (config.orchestrator_ip.as_str(), config.hyperloop_port),
            ("192.0.2.1", 5010)
        );
        assert_eq!(config.proxy_port, 5007);
        assert!(config.labels.is_empty());
        assert_eq!(config.template_storage, StorageSpec::local("/srv/builds"));
        assert_eq!(config.build_cache_storage, None);
        assert_eq!(config.upload_base_url(), "http://localhost:5008");
        assert_eq!(config.rootfs_exposure, Transport::Nbd);
    }

    #[test]
    fn cache_dirs_follow_the_go_variables_under_the_base_path() {
        let config = NodeConfig::from_vars(&vars(&[
            ("STORAGE_PROVIDER", "Local"),
            ("ORCHESTRATOR_BASE_PATH", "/data/orch"),
            ("TEMPLATE_CACHE_DIR", "/fast/template"),
        ]))
        .unwrap();

        assert_eq!(config.work_dir, PathBuf::from("/data/orch/sandbox"));
        assert_eq!(config.cache_dir, PathBuf::from("/data/orch/build"));
        assert_eq!(config.template_cache_dir, PathBuf::from("/fast/template"));
    }

    #[test]
    fn rootfs_exposure_comes_from_the_node_vars() {
        let config = |value: &str| {
            NodeConfig::from_vars(&vars(&[("STORAGE_PROVIDER", "Local"), ("ROOTFS_EXPOSURE", value)]))
                .map(|config| config.rootfs_exposure)
        };
        assert_eq!(config("ublk"), Ok(Transport::Ublk));
        assert_eq!(
            config("loop"),
            Err("invalid ROOTFS_EXPOSURE \"loop\": want nbd or ublk".to_owned())
        );
    }

    #[test]
    fn the_node_address_joins_the_node_ip_and_the_grpc_port() {
        let address = |extra: &[(&str, &str)]| {
            let base = [
                ("E2B_FIRECRACKER", "/f"),
                ("E2B_KERNEL", "/k"),
                ("STORAGE_PROVIDER", "Local"),
            ];
            let pairs: Vec<(&str, &str)> = base.iter().chain(extra).copied().collect();
            NodeConfig::from_vars(&vars(&pairs)).unwrap().node_address()
        };

        assert_eq!(address(&[]), None);
        assert_eq!(address(&[("NODE_IP", "10.0.0.7")]), Some("10.0.0.7:5008".to_owned()));
        assert_eq!(
            address(&[("NODE_IP", "fd00::7"), ("GRPC_PORT", "6000")]),
            Some("[fd00::7]:6000".to_owned())
        );
    }

    #[test]
    fn missing_or_bad_values_are_errors() {
        assert_eq!(
            NodeConfig::from_vars(&vars(&[("E2B_FIRECRACKER", "/f"), ("E2B_KERNEL", "/k")])).unwrap_err(),
            "failed to resolve template storage: template storage bucket not configured: set TEMPLATE_BUCKET_NAME"
        );
        assert_eq!(
            NodeConfig::from_vars(&vars(&[("GRPC_PORT", "x")])).unwrap_err(),
            "GRPC_PORT is not a port: x"
        );
    }

    #[test]
    fn the_firecracker_and_kernel_overrides_are_optional() {
        let bare = NodeConfig::from_vars(&vars(&[("STORAGE_PROVIDER", "Local")])).unwrap();
        let overridden = NodeConfig::from_vars(&vars(&[
            ("STORAGE_PROVIDER", "Local"),
            ("E2B_FIRECRACKER", "/f"),
            ("E2B_KERNEL", "/k"),
        ]))
        .unwrap();

        assert_eq!((bare.firecracker, bare.kernel), (PathBuf::new(), PathBuf::new()));
        assert_eq!(
            (overridden.firecracker, overridden.kernel),
            (PathBuf::from("/f"), PathBuf::from("/k"))
        );
    }

    #[test]
    fn environment_and_force_stop_follow_upstream() {
        let config = NodeConfig::from_vars(&vars(&[
            ("E2B_FIRECRACKER", "/f"),
            ("E2B_KERNEL", "/k"),
            ("STORAGE_PROVIDER", "Local"),
            ("ENVIRONMENT", "local"),
            ("FORCE_STOP", "T"),
        ]))
        .unwrap();
        assert!(config.local && config.force_stop);
        assert!(
            NodeConfig::from_vars(&vars(&[
                ("E2B_FIRECRACKER", "/f"),
                ("E2B_KERNEL", "/k"),
                ("STORAGE_PROVIDER", "Local"),
                ("FORCE_STOP", "yes")
            ]))
            .is_err()
        );
    }

    #[test]
    fn the_build_cache_resolves_only_for_the_template_manager() {
        let base = [
            ("E2B_FIRECRACKER", "/f"),
            ("E2B_KERNEL", "/k"),
            ("TEMPLATE_STORAGE_URL", "file:///t"),
            ("ORCHESTRATOR_SERVICES", "template-manager"),
        ];
        let config = NodeConfig::from_vars(&vars(&[&base[..], &[("STORAGE_PROVIDER", "Local")]].concat())).unwrap();
        assert_eq!(config.build_cache_storage, Some(StorageSpec::local("/tmp/build-cache")));
        assert_eq!(
            NodeConfig::from_vars(&vars(&base)).unwrap_err(),
            "failed to setup build storage: resolve build cache storage: build cache storage bucket not configured: set BUILD_CACHE_BUCKET_NAME"
        );
        let uploads = NodeConfig::from_vars(&vars(
            &[
                &base[..],
                &[
                    ("STORAGE_PROVIDER", "Local"),
                    ("LOCAL_UPLOAD_BASE_URL", "http://node:5008"),
                ],
            ]
            .concat(),
        ))
        .unwrap();
        assert_eq!(uploads.upload_base_url(), "http://node:5008");
    }
}
