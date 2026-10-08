use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tonic::Status;
use uuid::Uuid;

use super::admission::Admission;
use super::config::NodeConfig;
use super::lifecycle::{self, Command, SandboxHandle, SandboxInfo};
use super::registry::Registry;
use super::retry::Failure;
use super::status::NodeStatus;
use super::uploads::{Outcome, Uploads};
use crate::block::build_file::{Artifact, BuildFile, DiffStore};
use crate::block::overlay::Overlay;
use crate::envd::EnvdInit;
use crate::network::pool::NetworkPool;
use crate::network::slot::{HostSlots, Slot, host_slot_storage};
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;
use crate::sandbox::snapshot::LocalSnapshot;
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::template::cache::TemplateCache;
use crate::template::metadata::{MemoryPrefetch, TemplateMetadata};
use crate::template::{Template, TemplateError};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CGROUP_ROOT: &str = "/sys/fs/cgroup/e2b";

pub struct Node {
    pub node_id: String,
    pub storage: Storage,
    pub registry: Registry<SandboxHandle>,
    pub network: Arc<NetworkPool<HostSlots>>,
    pub local_expiry: bool,
    pub templates: Arc<TemplateCache>,
    pub labels: Vec<String>,
    pub status: Arc<NodeStatus>,
    pub uploads: Arc<Uploads>,
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
            status: Arc::default(),
            uploads: Arc::default(),
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
        })
    }

    pub fn start_background(&self) {
        self.network.start();
        tokio::spawn(self.templates.clone().run_eviction());
        tokio::spawn(self.store.clone().run_eviction(self.build_cache_max_usage));
    }

    pub async fn pause_locally(&self, sandbox: Sandbox, parent: &str, build_id: Uuid) -> Result<LocalSnapshot, String> {
        let dir = self.templates.instance_dir(build_id);
        let local = sandbox.pause_into(&dir, build_id).await?;
        let metadata = self.parent_metadata(parent).await.same_version(build_id);
        if let Err(e) = self.cache_snapshot(&local, metadata, dir) {
            tracing::warn!(build = %build_id, error = %e, "the paused snapshot is not cached on this node");
        }
        Ok(local)
    }

    pub fn upload(&self, local: LocalSnapshot) {
        let build = local.snapshot.build.clone();
        let storage = self.storage.clone();
        let store = self.store.clone();
        let attempt = move || {
            let local = local.clone();
            let storage = storage.clone();
            async move {
                if !local.files.exist() {
                    return Err(Failure::Terminal("snapshot files are gone".to_owned()));
                }
                let metadata = metadata_path(&local);
                let object = local.snapshot.build.paths().metadata();
                tokio::try_join!(local.upload(&storage), storage.put_file(&object, &metadata))
                    .map(|_| ())
                    .map_err(Failure::Retryable)
            }
        };
        self.uploads
            .start(build.build_id, parents(&build), attempt, move |outcome| {
                tracing::info!(build = %build.build_id, ?outcome, "snapshot upload finished");
                if outcome == Outcome::Landed {
                    store.unpin(build.build_id);
                }
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

    pub fn work_dir_of(&self, id: &str) -> PathBuf {
        self.work_dir.join(id)
    }

    pub async fn create(self: Arc<Self>, config: SandboxConfig, lifetime: Lifetime) -> Result<(), Status> {
        let _work = self.status.track_work();
        let id = config.sandbox_id.clone();
        self.registry.reserve(&id).map_err(Status::already_exists)?;
        let started = async {
            let _permit = self.admission.admit(self.registry.running().len()).await?;
            self.start(config, lifetime).await
        };
        match started.await {
            Ok(handle) => {
                self.registry.activate(&id, handle);
                Ok(())
            }
            Err(status) => {
                self.registry.abandon(&id);
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

    async fn start(self: &Arc<Self>, config: SandboxConfig, lifetime: Lifetime) -> Result<SandboxHandle, Status> {
        let template = self.template_of(&config)?;
        let (files, slot) = tokio::join!(
            async { tokio::try_join!(template.build(), template.snapfile(), template.metadata()) },
            tokio::time::timeout(REQUEST_TIMEOUT, self.network.get())
        );
        let slot = slot
            .unwrap_or_else(|_| Err("request timed out".to_owned()))
            .map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))?;
        let (build, snapfile, metadata) = files.map_err(|e| {
            self.network.return_slot(slot);
            template_status(&config, e)
        })?;
        let prefetch = self
            .prefetch_from_trace
            .then(|| metadata.memory_prefetch().cloned())
            .flatten();
        match self.resume(&config, &build, &snapfile, prefetch, slot).await {
            Ok(sandbox) => Ok(lifecycle::spawn(
                sandbox,
                slot,
                SandboxInfo {
                    config,
                    start: lifetime.start,
                    end: lifetime.end,
                },
                self.clone(),
            )),
            Err(e) => {
                self.network.return_slot(slot);
                let _ = std::fs::remove_dir_all(self.work_dir_of(&config.sandbox_id));
                Err(Status::internal(format!("failed to create sandbox: {e}")))
            }
        }
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
    ) -> Result<Sandbox, String> {
        let work_dir = self.work_dir_of(&config.sandbox_id);
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
            .envd(EnvdInit {
                access_token: config.envd_access_token.clone(),
                env_vars: config.env_vars.clone().into_iter().collect::<BTreeMap<_, _>>(),
                ..EnvdInit::default()
            })
            .resume(snapfile)
            .await
    }
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
