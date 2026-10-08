mod checkpoint;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use tonic::Status;
use uuid::Uuid;

pub use self::checkpoint::CheckpointRequest;
use super::admission::Admission;
use super::build_id;
use super::config::NodeConfig;
use super::lifecycle::{self, Command, SandboxHandle, SandboxInfo};
use super::oom_monitor::SharedWatermark;
use super::registry::{Lifecycle, Registry};
use super::retry::Failure;
use super::scheduling::from_headers;
use super::services::Services;
use super::status::NodeStatus;
use super::uploads::Uploads;
use crate::block::build_file::{Artifact, BuildFile, DiffStore};
use crate::block::overlay::Overlay;
use crate::clickhouse::sinks::ClickhouseHostStats;
use crate::envd::{EnvdInit, VolumeMount};
use crate::events::{CREATED, EventSandbox, EventsService, Execution, RESUMED, SandboxEvent};
use crate::firecracker::version::supports_memfd;
use crate::hoststats::StatsIdentity;
use crate::hyperloop::logs::SourceSandbox;
use crate::network::firewall::EgressRules;
use crate::network::pool::NetworkPool;
use crate::network::slot::{HostSlots, Slot, host_slot_storage};
use crate::proto::{SandboxConfig, SandboxNetworkEgressConfig, SchedulingMetadata};
use crate::routing::publisher::{RoutedSandbox, RoutingPublisher};
use crate::sandbox::Sandbox;
use crate::sandbox::config::SandboxBuilder;
use crate::sandbox::snapshot::LocalSnapshot;
use crate::storage::backend::{ObjectMetadata, Storage};
use crate::storage::build::Build;
use crate::tcpfirewall::proxy::FirewallSandbox;
use crate::template::cache::TemplateCache;
use crate::template::metadata::{MemoryPrefetch, TemplateMetadata, TemplateVersions};
use crate::template::{Template, TemplateError};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const DEADLINE_EXCEEDED: &str = "context deadline exceeded\nrequest timed out";
const TEMPLATE_DEFAULT_USER: &str = "user";
const CGROUP_ROOT: &str = "/sys/fs/cgroup/e2b";

pub struct Node {
    pub node_id: String,
    pub storage: Storage,
    pub registry: Registry<SandboxHandle>,
    pub network: Arc<NetworkPool<HostSlots>>,
    pub local_expiry: bool,
    pub templates: Arc<TemplateCache>,
    pub labels: Vec<String>,
    pub services: Services,
    pub status: Arc<NodeStatus>,
    pub uploads: Arc<Uploads>,
    build_network: Arc<Mutex<HashMap<Ipv4Addr, String>>>,
    pub routing: RoutingPublisher,
    pub events: EventsService,
    pub host_stats: OnceLock<Arc<ClickhouseHostStats>>,
    admission: Admission,
    pub max_sandboxes: i64,
    store: Arc<DiffStore>,
    firecracker: PathBuf,
    kernel: PathBuf,
    work_dir: PathBuf,
    envd_timeout: Duration,
    build_cache_max_usage: f64,
    prefetch_from_trace: bool,
    use_memfd: bool,
    orchestrator_ip: String,
}

struct ResumeRequest<'a> {
    config: &'a SandboxConfig,
    build: &'a Build,
    snapfile: &'a Path,
    prefetch: Option<MemoryPrefetch>,
    slot: Slot,
    init: EnvdInit,
    lifecycle: &'a Lifecycle,
}

struct StartRequest {
    template: Arc<Template>,
    config: SandboxConfig,
    lifetime: Lifetime,
    lifecycle: Lifecycle,
    oom_watermark: SharedWatermark,
}

enum StartFailure {
    Rejected(Status),
    Template(TemplateError),
    Failed(String),
}

impl StartFailure {
    fn create_status(self, sandbox_id: &str) -> Status {
        match self {
            StartFailure::Rejected(status) => status,
            StartFailure::Template(TemplateError::NotFound(_)) => {
                Status::failed_precondition(format!("sandbox files for '{sandbox_id}' not found"))
            }
            StartFailure::Template(TemplateError::Failed(e)) | StartFailure::Failed(e) => {
                Status::internal(format!("failed to create sandbox: {e}"))
            }
        }
    }

    fn message(&self) -> String {
        match self {
            StartFailure::Rejected(status) => status.message().to_owned(),
            StartFailure::Template(e) => e.to_string(),
            StartFailure::Failed(e) => e.clone(),
        }
    }
}

pub struct BuildNetwork {
    map: Arc<Mutex<HashMap<Ipv4Addr, String>>>,
    host_ip: Ipv4Addr,
}

impl Drop for BuildNetwork {
    fn drop(&mut self) {
        self.map.lock().remove(&self.host_ip);
    }
}

pub struct Lifetime {
    pub start: SystemTime,
    pub end: SystemTime,
}

impl Node {
    pub fn new(config: &NodeConfig) -> Result<Self, String> {
        let storage = config.open_template_storage()?;
        Ok(Self {
            node_id: config.node_id.clone(),
            store: Arc::new(DiffStore::new(storage.clone(), config.cache_dir.clone())),
            labels: config.labels.clone(),
            services: config.services.clone(),
            status: Arc::default(),
            uploads: Arc::default(),
            build_network: Arc::default(),
            events: EventsService::default(),
            host_stats: OnceLock::new(),
            routing: RoutingPublisher::new(None, Uuid::new_v4().to_string(), config.node_ip.clone()),
            admission: Admission::new(config.max_sandboxes, config.max_starting),
            max_sandboxes: config.max_sandboxes,
            templates: Arc::new(TemplateCache::new(storage.clone(), config.template_cache_dir.clone())),
            storage,
            registry: Registry::new(),
            network: NetworkPool::new(HostSlots, host_slot_storage(config.slots.clone())),
            local_expiry: config.local_expiry,
            firecracker: config.firecracker.clone(),
            kernel: config.kernel.clone(),
            work_dir: config.work_dir.clone(),
            envd_timeout: config.envd_timeout,
            build_cache_max_usage: config.build_cache_max_usage,
            prefetch_from_trace: config.prefetch_from_trace,
            use_memfd: config.use_memfd,
            orchestrator_ip: config.orchestrator_ip.clone(),
        })
    }

    pub fn start_background(&self) {
        self.network.start();
        tokio::spawn(self.templates.clone().run_eviction());
        tokio::spawn(self.store.clone().run_eviction(self.build_cache_max_usage));
    }

    pub async fn snapshot_locally(
        &self,
        sandbox: &Sandbox,
        parent: &SandboxInfo,
        build_id: Uuid,
    ) -> Result<LocalSnapshot, String> {
        let dir = self.templates.instance_dir(build_id);
        let local = sandbox.snapshot_into(&dir, build_id).await?;
        let metadata = match &parent.metadata {
            Some(metadata) => metadata.clone(),
            None => self.parent_metadata(&parent.config.build_id).await,
        }
        .same_version(TemplateVersions {
            build_id: build_id.to_string(),
            kernel_version: parent.config.kernel_version.clone(),
            firecracker_version: parent.config.firecracker_version.clone(),
        });
        if let Err(e) = self.cache_snapshot(&local, metadata, dir) {
            tracing::warn!(build = %build_id, error = %e, "the paused snapshot is not cached on this node");
        }
        Ok(local)
    }

    pub fn upload(&self, local: LocalSnapshot, object_metadata: ObjectMetadata) {
        let build = local.snapshot.build.clone();
        let storage = self.storage.clone();
        let store = self.store.clone();
        let attempt = move || upload_attempt(local.clone(), storage.clone(), object_metadata.clone());
        self.uploads
            .start(build.build_id, parents(&build), attempt, move |outcome| {
                tracing::info!(build = %build.build_id, ?outcome, "snapshot upload finished");
                store.unpin(build.build_id);
            });
    }

    pub async fn upload_now(&self, local: LocalSnapshot, object_metadata: ObjectMetadata) -> Result<(), String> {
        let build = local.snapshot.build.clone();
        let attempt = upload_attempt(local, self.storage.clone(), object_metadata);
        let uploads = self.uploads.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let uploaded = uploads.run(build.build_id, parents(&build), attempt).await;
            store.unpin(build.build_id);
            uploaded
        })
        .await
        .map_err(|e| format!("snapshot upload task failed: {e}"))?
    }

    pub fn build_sandbox(&self, sandbox_id: &str, template_id: &str) -> SandboxBuilder {
        Sandbox::builder()
            .id(sandbox_id)
            .template_id(template_id)
            .firecracker(&self.firecracker)
            .kernel(&self.kernel)
            .work_dir(self.work_dir.join(sandbox_id))
    }

    pub fn layer_dir(&self, build_id: Uuid) -> PathBuf {
        self.templates.instance_dir(build_id)
    }

    pub fn cache_layer(&self, local: &LocalSnapshot, metadata: TemplateMetadata, dir: PathBuf) -> Result<(), String> {
        self.cache_snapshot(local, metadata, dir)
    }

    fn cache_snapshot(&self, local: &LocalSnapshot, metadata: TemplateMetadata, dir: PathBuf) -> Result<(), String> {
        let build_id = local.snapshot.build.build_id;
        let metadata_file = metadata_path(local);
        std::fs::write(&metadata_file, metadata.to_json())
            .map_err(|e| format!("write {}: {e}", metadata_file.display()))?;
        self.store.add_local_when(
            build_id,
            Artifact::Memfile,
            &local.files.memfile,
            local.memfile_ready.clone(),
        );
        self.store.add_local(build_id, Artifact::Rootfs, &local.files.rootfs)?;
        self.templates.add_snapshot(
            local.snapshot.build.clone(),
            local.files.snapfile.clone(),
            metadata,
            dir,
        );
        Ok(())
    }

    async fn parent_metadata(&self, parent: &str) -> TemplateMetadata {
        match Uuid::parse_str(parent) {
            Ok(parent) => self.templates.get(parent).metadata().await.unwrap_or_else(|e| {
                tracing::warn!(build = %parent, error = %e, "parent metadata is unreadable; writing v1 metadata");
                TemplateMetadata::v1()
            }),
            Err(_) => TemplateMetadata::v1(),
        }
    }

    pub fn source_of(&self, ip: IpAddr) -> Option<SourceSandbox> {
        self.registry
            .running()
            .iter()
            .map(|handle| handle.info.lock().clone())
            .find(|info| IpAddr::V4(info.host_ip) == ip)
            .map(|info| SourceSandbox {
                sandbox_id: info.config.sandbox_id,
                template_id: info.config.template_id,
                team_id: info.config.team_id,
                build_id: info.config.build_id,
                lifecycle_started_at: Some(info.started_at),
            })
    }

    pub fn assign_build_network(&self, host_ip: Ipv4Addr, sandbox_id: &str) -> BuildNetwork {
        self.build_network.lock().insert(host_ip, sandbox_id.to_owned());
        BuildNetwork {
            map: self.build_network.clone(),
            host_ip,
        }
    }

    pub fn firewall_source(&self, ip: IpAddr) -> Option<FirewallSandbox> {
        let build = match ip {
            IpAddr::V4(v4) => self.build_network.lock().get(&v4).cloned(),
            IpAddr::V6(_) => None,
        };
        if let Some(sandbox_id) = build {
            return Some(FirewallSandbox {
                sandbox_id,
                egress: None,
            });
        }
        self.registry
            .running()
            .iter()
            .map(|handle| handle.info.lock().clone())
            .find(|info| IpAddr::V4(info.host_ip) == ip)
            .map(|info| FirewallSandbox {
                egress: info.config.network.and_then(|network| network.egress),
                sandbox_id: info.config.sandbox_id,
            })
    }

    pub fn running_configs(&self) -> Vec<SandboxConfig> {
        self.registry
            .running()
            .iter()
            .map(|handle| handle.info.lock().config.clone())
            .collect()
    }

    pub async fn close(&self) {
        self.uploads.wait_all().await;
        self.network.close().await;
    }

    pub fn work_dir_of(&self, lifecycle: &Lifecycle) -> PathBuf {
        self.work_dir
            .join(format!("{}-{}", lifecycle.sandbox_id, lifecycle.lifecycle_id))
    }

    pub async fn create(
        self: Arc<Self>,
        config: SandboxConfig,
        lifetime: Lifetime,
    ) -> Result<SchedulingMetadata, Status> {
        let _work = self.status.track_work();
        let lifecycle = Lifecycle {
            sandbox_id: config.sandbox_id.clone(),
            lifecycle_id: Uuid::new_v4().to_string(),
            execution_id: config.execution_id.clone(),
        };
        let id = lifecycle.sandbox_id.clone();
        self.registry.reserve(&id).map_err(Status::already_exists)?;
        let started = async {
            let _permit = self.admission.admit(self.registry.running().len()).await?;
            let template = self.template_of(&config)?;
            self.start(StartRequest {
                template,
                config,
                lifetime,
                lifecycle: lifecycle.clone(),
                oom_watermark: SharedWatermark::default(),
            })
            .await
            .map_err(|failure| failure.create_status(&id))
        };
        let started = tokio::time::timeout(REQUEST_TIMEOUT, started)
            .await
            .unwrap_or_else(|_| {
                Err(Status::internal(format!(
                    "failed to create sandbox: {DEADLINE_EXCEEDED}"
                )))
            });
        let created = match started {
            Ok(handle) => self
                .register(&lifecycle, &handle)
                .await
                .map_err(|e| Status::internal(format!("failed to register sandbox: {e}")))
                .map(|info| {
                    let kind = if info.config.snapshot { RESUMED } else { CREATED };
                    self.events.publish(SandboxEvent::new(kind, &event_sandbox(&info)));
                    info.scheduling
                }),
            Err(status) => {
                let _ = std::fs::remove_dir_all(self.work_dir_of(&lifecycle));
                Err(status)
            }
        };
        self.registry.release(&id);
        created
    }

    async fn register(&self, lifecycle: &Lifecycle, handle: &SandboxHandle) -> Result<SandboxInfo, String> {
        let info = handle.info.lock().clone();
        let registered = self
            .registry
            .activate(lifecycle.clone(), handle.clone(), || handle.cleanup_started());
        if let Err(e) = registered {
            let _ = handle.commands.send(Command::Delete).await;
            return Err(e);
        }
        self.routing.on_insert(&routed_sandbox(&info)).await;
        Ok(info)
    }

    pub async fn mark_stopping(&self, lifecycle: &Lifecycle) -> Option<SandboxHandle> {
        let stopped = self.registry.stop(lifecycle)?;
        let routed = routed_sandbox(&stopped.info.lock());
        self.routing.on_stopping(&routed).await;
        Some(stopped)
    }

    pub async fn shutdown(&self) {
        for handle in self.registry.running() {
            if let Some(stopped) = self.mark_stopping(&handle.lifecycle()).await {
                let _ = stopped.commands.send(Command::Delete).await;
            }
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        while !self.registry.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn sweep(&self) {
        let namespaces = crate::system::output("ip", &["netns", "list"])
            .await
            .unwrap_or_default();
        let slots: Vec<Slot> = namespaces
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter_map(|name| name.strip_prefix("ns-"))
            .filter_map(|index| index.parse().ok())
            .filter_map(|index| Slot::new(index).ok())
            .collect();
        for slot in slots {
            if let Err(e) = slot.destroy().await {
                tracing::warn!(slot = slot.index, error = %e, "could not remove a leftover slot");
            }
        }
        sweep_cgroups(Path::new(CGROUP_ROOT));
        let _ = std::fs::remove_dir_all(&self.work_dir);
    }

    async fn start(self: &Arc<Self>, request: StartRequest) -> Result<SandboxHandle, StartFailure> {
        let StartRequest {
            template,
            config,
            lifetime,
            lifecycle,
            oom_watermark,
        } = request;
        let files = async { tokio::try_join!(template.build(), template.snapfile()) };
        let egress = egress_rules(config.network.as_ref().and_then(|network| network.egress.as_ref()))
            .map_err(StartFailure::Rejected)?;
        let lease = self.network.lease(egress.as_ref());
        tokio::pin!(files, lease);
        let (files, lease) = tokio::select! {
            files = &mut files => match files {
                Err(e) => return Err(StartFailure::Template(e)),
                files => (files, lease.await),
            },
            lease = &mut lease => (files.await, lease),
        };
        let lease = lease.map_err(StartFailure::Failed)?;
        let (build, snapfile) = files.map_err(StartFailure::Template)?;
        let slot = lease.slot().ok_or_else(slot_taken_back)?;
        let metadata = template
            .metadata()
            .await
            .inspect_err(|e| tracing::warn!(build = %build.build_id, error = %e, "metadata.json is unreadable; resuming without prefetch"))
            .ok();
        let prefetch = self
            .prefetch_from_trace
            .then(|| metadata.as_ref().and_then(TemplateMetadata::memory_prefetch).cloned())
            .flatten();
        let init = EnvdInit {
            access_token: config.envd_access_token.clone(),
            env_vars: config.env_vars.clone().into_iter().collect::<BTreeMap<_, _>>(),
            lifecycle_id: Some(lifecycle.lifecycle_id.clone()),
            hyperloop_ip: Some(self.orchestrator_ip.clone()),
            volume_mounts: volume_mounts(&config, &self.orchestrator_ip).map_err(StartFailure::Failed)?,
            default_user: default_user(metadata.as_ref()),
            ..EnvdInit::default()
        };
        let sandbox = self
            .resume(ResumeRequest {
                config: &config,
                build: &build,
                snapfile: &snapfile,
                prefetch,
                slot,
                init,
                lifecycle: &lifecycle,
            })
            .await
            .map_err(StartFailure::Failed)?;
        let slot = lease.keep().ok_or_else(slot_taken_back)?;
        Ok(lifecycle::spawn(
            sandbox,
            slot,
            SandboxInfo {
                config,
                start: lifetime.start,
                end: lifetime.end,
                metadata,
                lifecycle,
                host_ip: slot.host_ip(),
                slot,
                template: template.clone(),
                scheduling: from_headers(build.rootfs.metadata.build_id, Some(&build.memfile), &build.rootfs, 0),
                started_at: SystemTime::now(),
                oom_watermark,
            },
            self.clone(),
        ))
    }

    fn template_of(&self, config: &SandboxConfig) -> Result<Arc<Template>, Status> {
        build_id::parse(&config.build_id)
            .map(|build_id| self.templates.get_for(build_id, config.max_sandbox_length))
            .map_err(|_| {
                StartFailure::Template(TemplateError::NotFound(config.build_id.clone()))
                    .create_status(&config.sandbox_id)
            })
    }

    async fn resume(&self, request: ResumeRequest<'_>) -> Result<Sandbox, String> {
        let ResumeRequest {
            config,
            build,
            snapfile,
            prefetch,
            slot,
            init,
            lifecycle,
        } = request;
        let work_dir = self.work_dir_of(lifecycle);
        std::fs::create_dir_all(&work_dir).map_err(|e| format!("create {}: {e}", work_dir.display()))?;
        let rootfs = Arc::new(BuildFile::new(
            build.rootfs.clone(),
            Artifact::Rootfs,
            self.store.clone(),
        ));
        let overlay = Overlay::new(rootfs, &work_dir.join("rootfs.overlay"))
            .map_err(|e| format!("create rootfs overlay: {e}"))?;
        let memfile = Arc::new(BuildFile::new(
            build.memfile.clone(),
            Artifact::Memfile,
            self.store.clone(),
        ));
        Sandbox::builder()
            .id(&config.sandbox_id)
            .template_id(&config.template_id)
            .rootfs(Arc::new(overlay))
            .memfile(memfile)
            .pooled_slot(slot)
            .firecracker(&self.firecracker)
            .kernel(&self.kernel)
            .work_dir(work_dir)
            .envd_timeout(self.envd_timeout)
            .prefetch(prefetch)
            .use_memfd(self.use_memfd && supports_memfd(&config.firecracker_version))
            .hyperloop_ip(&self.orchestrator_ip)
            .envd(init)
            .resume(snapfile)
            .await
    }
}

pub fn egress_rules(egress: Option<&SandboxNetworkEgressConfig>) -> Result<Option<EgressRules>, Status> {
    let Some(egress) = egress else {
        return Ok(None);
    };
    if !egress.egress_proxy_address.is_empty() {
        return Err(Status::permission_denied("egress proxy is not enabled for this team"));
    }
    let unrestricted =
        egress.allowed_cidrs.is_empty() && egress.denied_cidrs.is_empty() && egress.allowed_domains.is_empty();
    Ok((!unrestricted).then(|| EgressRules {
        byop: false,
        allowed: egress.allowed_cidrs.clone(),
        denied: egress.denied_cidrs.clone(),
    }))
}

pub fn event_sandbox(info: &SandboxInfo) -> EventSandbox {
    EventSandbox {
        sandbox_id: info.config.sandbox_id.clone(),
        execution_id: info.config.execution_id.clone(),
        base_template_id: info.config.base_template_id.clone(),
        build_id: info.config.build_id.clone(),
        team_id: info.config.team_id.clone(),
        events_ttl_days: info.config.events_ttl_days,
        metadata: info.config.metadata.clone().into_iter().collect(),
    }
}

pub fn stats_identity(info: &SandboxInfo) -> StatsIdentity {
    StatsIdentity {
        sandbox_id: info.config.sandbox_id.clone(),
        execution_id: info.config.execution_id.clone(),
        template_id: info.config.template_id.clone(),
        build_id: info.config.build_id.clone(),
        team_id: info.config.team_id.clone(),
        vcpu_count: info.config.vcpu,
        memory_mb: info.config.ram_mb,
    }
}

pub fn execution(info: &SandboxInfo) -> Execution {
    Execution {
        started_at: info.start,
        vcpu_count: info.config.vcpu,
        memory_mb: info.config.ram_mb,
    }
}

pub fn routed_sandbox(info: &SandboxInfo) -> RoutedSandbox {
    RoutedSandbox {
        sandbox_id: info.config.sandbox_id.clone(),
        lifecycle_id: info.lifecycle.lifecycle_id.clone(),
        execution_id: info.config.execution_id.clone(),
        started_at: info.start,
        max_length_hours: info.config.max_sandbox_length,
    }
}

fn volume_mounts(config: &SandboxConfig, orchestrator_ip: &str) -> Result<Vec<VolumeMount>, String> {
    config
        .volume_mounts
        .iter()
        .map(|mount| {
            Uuid::parse_str(&mount.id)
                .map(|_| VolumeMount {
                    nfs_target: format!("{orchestrator_ip}:/{}", mount.name),
                    path: mount.path.clone(),
                })
                .map_err(|e| format!("failed to convert volume mounts: invalid volume id {:?}: {e}", mount.id))
        })
        .collect()
}

fn default_user(metadata: Option<&TemplateMetadata>) -> Option<String> {
    metadata
        .and_then(|metadata| metadata.context.user.as_deref())
        .filter(|user| *user == TEMPLATE_DEFAULT_USER)
        .map(str::to_owned)
}

async fn upload_attempt(
    local: LocalSnapshot,
    storage: Storage,
    object_metadata: ObjectMetadata,
) -> Result<(), Failure> {
    if !local.files.exist() {
        return Err(Failure::Terminal("snapshot files are gone".to_owned()));
    }
    let metadata = metadata_path(&local);
    let object = local.snapshot.build.paths().metadata();
    tokio::try_join!(
        local.upload(&storage, &object_metadata),
        storage.put_file_with(&object, &metadata, &object_metadata)
    )
    .map(|_| ())
    .map_err(Failure::Retryable)
}

fn metadata_path(local: &LocalSnapshot) -> PathBuf {
    local.files.snapfile.with_file_name("metadata.json")
}

fn parents(build: &Build) -> Vec<Uuid> {
    build
        .memfile
        .mappings
        .iter()
        .chain(&build.rootfs.mappings)
        .map(|mapping| mapping.build_id)
        .filter(|parent| !parent.is_nil() && *parent != build.build_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn slot_taken_back() -> StartFailure {
    StartFailure::Failed("the network slot was taken back".to_owned())
}

fn sweep_cgroups(root: &Path) {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .for_each(|cgroup| {
            let _ = std::fs::write(cgroup.join("cgroup.kill"), "1");
            let _ = std::fs::remove_dir(&cgroup);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::metadata::Context;

    fn metadata(context_user: Option<&str>, start_user: &str) -> TemplateMetadata {
        TemplateMetadata {
            context: Context {
                user: context_user.map(str::to_owned),
                ..Context::default()
            },
            start: Some(serde_json::json!({ "context": { "user": start_user } })),
            ..TemplateMetadata::default()
        }
    }

    #[test]
    fn the_default_user_comes_from_the_template_context_like_upstream() {
        assert_eq!(
            default_user(Some(&metadata(Some("user"), "root"))),
            Some("user".to_owned())
        );
        assert_eq!(default_user(Some(&metadata(Some("root"), "user"))), None);
        assert_eq!(default_user(Some(&metadata(None, "user"))), None);
        assert_eq!(default_user(None), None);
    }
}
