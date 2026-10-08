use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tonic::Status;
use uuid::Uuid;

use super::admission::Admission;
use super::config::NodeConfig;
use super::lifecycle::{self, Command, SandboxHandle, SandboxInfo};
use super::registry::{Lifecycle, Registry};
use super::retry::Failure;
use super::services::Services;
use super::status::NodeStatus;
use super::uploads::Uploads;
use crate::block::build_file::{Artifact, BuildFile, DiffStore};
use crate::block::overlay::Overlay;
use crate::envd::{EnvdInit, VolumeMount};
use crate::events::{CREATED, EventSandbox, EventsService, Execution, RESUMED, SandboxEvent};
use crate::hyperloop::logs::SourceSandbox;
use crate::network::firewall::EgressRules;
use crate::network::pool::NetworkPool;
use crate::network::slot::{HostSlots, Slot, host_slot_storage};
use crate::proto::{SandboxConfig, SandboxNetworkEgressConfig};
use crate::routing::publisher::{RoutedSandbox, RoutingPublisher};
use crate::sandbox::Sandbox;
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
    pub routing: RoutingPublisher,
    pub events: EventsService,
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

pub struct Lifetime {
    pub start: SystemTime,
    pub end: SystemTime,
}

impl Node {
    pub fn new(config: &NodeConfig) -> Result<Self, String> {
        let storage = config.open_storage()?;
        Ok(Self {
            node_id: config.node_id.clone(),
            store: Arc::new(DiffStore::new(storage.clone(), config.cache_dir.clone())),
            labels: config.labels.clone(),
            services: config.services.clone(),
            status: Arc::default(),
            uploads: Arc::default(),
            events: EventsService::default(),
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
        let attempt = move || {
            let local = local.clone();
            let storage = storage.clone();
            let object_metadata = object_metadata.clone();
            async move {
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
        };
        self.uploads
            .start(build.build_id, parents(&build), attempt, move |outcome| {
                tracing::info!(build = %build.build_id, ?outcome, "snapshot upload finished");
                store.unpin(build.build_id);
            });
    }

    fn cache_snapshot(&self, local: &LocalSnapshot, metadata: TemplateMetadata, dir: PathBuf) -> Result<(), String> {
        let build_id = local.snapshot.build.build_id;
        let metadata_file = metadata_path(local);
        std::fs::write(&metadata_file, metadata.to_json())
            .map_err(|e| format!("write {}: {e}", metadata_file.display()))?;
        self.store
            .add_local(build_id, Artifact::Memfile, &local.files.memfile)?;
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

    pub fn firewall_source(&self, ip: IpAddr) -> Option<FirewallSandbox> {
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

    pub async fn create(self: Arc<Self>, config: SandboxConfig, lifetime: Lifetime) -> Result<(), Status> {
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
            self.start(config, lifetime, lifecycle.clone()).await
        };
        let started = tokio::time::timeout(REQUEST_TIMEOUT, started)
            .await
            .unwrap_or_else(|_| {
                Err(Status::internal(format!(
                    "failed to create sandbox: {DEADLINE_EXCEEDED}"
                )))
            });
        match started {
            Ok(handle) => {
                let info = handle.info.lock().clone();
                self.registry.activate(lifecycle, handle);
                self.routing.on_insert(&routed_sandbox(&info)).await;
                let kind = if info.config.snapshot { RESUMED } else { CREATED };
                self.events.publish(SandboxEvent::new(kind, &event_sandbox(&info)));
                Ok(())
            }
            Err(status) => {
                self.registry.abandon(&id);
                let _ = std::fs::remove_dir_all(self.work_dir_of(&lifecycle));
                Err(status)
            }
        }
    }

    pub async fn shutdown(&self) {
        self.registry
            .running()
            .iter()
            .filter_map(|handle| {
                self.registry
                    .stop(&handle.info.lock().config.sandbox_id)
                    .map(|_| handle)
            })
            .for_each(|handle| drop(handle.commands.try_send(Command::Delete)));
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

    async fn start(
        self: &Arc<Self>,
        config: SandboxConfig,
        lifetime: Lifetime,
        lifecycle: Lifecycle,
    ) -> Result<SandboxHandle, Status> {
        let template = self.template_of(&config)?;
        let files = async { tokio::try_join!(template.build(), template.snapfile()) };
        let egress = egress_rules(config.network.as_ref().and_then(|network| network.egress.as_ref()))?;
        let lease = self.network.lease(egress.as_ref());
        tokio::pin!(files, lease);
        let (files, lease) = tokio::select! {
            files = &mut files => match files {
                Err(e) => return Err(template_status(&config, e)),
                files => (files, lease.await),
            },
            lease = &mut lease => (files.await, lease),
        };
        let lease = lease.map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))?;
        let (build, snapfile) = files.map_err(|e| template_status(&config, e))?;
        let slot = lease
            .slot()
            .ok_or_else(|| Status::internal("failed to create sandbox: the network slot was taken back"))?;
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
            volume_mounts: volume_mounts(&config, &self.orchestrator_ip)
                .map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))?,
            default_user: default_user(metadata.as_ref()),
            ..EnvdInit::default()
        };
        let sandbox = self
            .resume(&config, &build, &snapfile, prefetch, slot, init, &lifecycle)
            .await
            .map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))?;
        let slot = lease
            .keep()
            .ok_or_else(|| Status::internal("failed to create sandbox: the network slot was taken back"))?;
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
                started_at: SystemTime::now(),
            },
            self.clone(),
        ))
    }

    fn template_of(&self, config: &SandboxConfig) -> Result<Arc<Template>, Status> {
        Uuid::parse_str(&config.build_id)
            .map(|build_id| self.templates.get(build_id))
            .map_err(|e| Status::invalid_argument(format!("invalid build id '{}': {e}", config.build_id)))
    }

    async fn resume(
        &self,
        config: &SandboxConfig,
        build: &Build,
        snapfile: &Path,
        prefetch: Option<MemoryPrefetch>,
        slot: Slot,
        init: EnvdInit,
        lifecycle: &Lifecycle,
    ) -> Result<Sandbox, String> {
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
            .use_memfd(self.use_memfd)
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
        .and_then(|metadata| metadata.start.as_ref())
        .and_then(|start| start.pointer("/context/user"))
        .and_then(serde_json::Value::as_str)
        .filter(|user| *user == "user")
        .map(str::to_owned)
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

fn template_status(config: &SandboxConfig, error: TemplateError) -> Status {
    match error {
        TemplateError::NotFound(_) => {
            Status::failed_precondition(format!("sandbox files for '{}' not found", config.sandbox_id))
        }
        TemplateError::Failed(e) => Status::internal(format!("failed to create sandbox: {e}")),
    }
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
