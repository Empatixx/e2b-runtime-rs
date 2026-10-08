use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use tonic::Status;
use uuid::Uuid;

use super::config::NodeConfig;
use super::lifecycle::{self, SandboxHandle, SandboxInfo};
use super::registry::Registry;
use super::slots::SlotPool;
use crate::block::build_file::{Artifact, BuildFile, DiffStore};
use crate::block::overlay::Overlay;
use crate::envd::EnvdInit;
use crate::network::slot::Slot;
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::storage::paths::BuildPaths;

pub struct Node {
    pub node_id: String,
    pub storage: Storage,
    pub registry: Registry<SandboxHandle>,
    pub slots: SlotPool,
    store: Arc<DiffStore>,
    firecracker: PathBuf,
    kernel: PathBuf,
    work_dir: PathBuf,
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
            storage,
            registry: Registry::new(),
            slots: SlotPool::new(config.slots.clone()),
            firecracker: config.firecracker.clone(),
            kernel: config.kernel.clone(),
            work_dir: config.work_dir.clone(),
        })
    }

    pub async fn start(self: &Arc<Self>, config: SandboxConfig, lifetime: Lifetime) -> Result<SandboxHandle, Status> {
        let build = self.build_of(&config).await?;
        let slot = self.slots.acquire().map_err(Status::resource_exhausted)?;
        match self.resume(&config, &build, slot).await {
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
                self.slots.release(slot);
                Err(Status::internal(format!("failed to create sandbox: {e}")))
            }
        }
    }

    async fn build_of(&self, config: &SandboxConfig) -> Result<Build, Status> {
        let build_id = Uuid::parse_str(&config.build_id)
            .map_err(|e| Status::invalid_argument(format!("invalid build id '{}': {e}", config.build_id)))?;
        let present = self
            .storage
            .exists(&BuildPaths::new(build_id).memfile_header())
            .await
            .map_err(Status::internal)?;
        if !present {
            return Err(Status::failed_precondition(format!(
                "sandbox files for '{}' not found",
                config.sandbox_id
            )));
        }
        Build::load(&self.storage, build_id)
            .await
            .map_err(|e| Status::internal(format!("failed to create sandbox: {e}")))
    }

    async fn resume(&self, config: &SandboxConfig, build: &Build, slot: Slot) -> Result<Sandbox, String> {
        let work_dir = self.work_dir.join(&config.sandbox_id);
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
            .slot(slot)
            .firecracker(&self.firecracker)
            .kernel(&self.kernel)
            .work_dir(work_dir)
            .envd(EnvdInit {
                access_token: config.envd_access_token.clone(),
                env_vars: config.env_vars.clone().into_iter().collect::<BTreeMap<_, _>>(),
                ..EnvdInit::default()
            })
            .resume(build, &self.storage)
            .await
    }
}
