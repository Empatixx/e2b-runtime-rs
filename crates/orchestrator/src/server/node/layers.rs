use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use uuid::Uuid;

use super::{BUILD_SANDBOX_TYPE, BaseSandbox, Node, parents};
use crate::block::build_file::{Artifact, BuildFile};
use crate::block::overlay::Overlay;
use crate::envd::{EnvdDefaults, EnvdInit};
use crate::featureflags::FlagContext;
use crate::firecracker::version::supports_memfd;
use crate::sandbox::config::SandboxBuilder;
use crate::sandbox::config::sandbox_builder::{
    SetMemfile, SetMemoryBase, SetPrefetch, SetPrefetchSettings, SetRootfs, SetUseMemfd,
};
use crate::sandbox::snapshot::filesystem::FilesystemSnapshot;
use crate::server::retry::Failure;
use crate::storage::backend::{ObjectMetadata, Storage};
use crate::storage::build::Build;
use crate::storage::header::Header;
use crate::template::Template;
use crate::template::metadata::TemplateMetadata;

pub struct LayerSource {
    pub template: Arc<Template>,
    pub build: Build,
    pub snapfile: Option<PathBuf>,
    pub metadata: TemplateMetadata,
}

pub struct LayerSandbox<'a> {
    pub source: &'a LayerSource,
    pub sandbox_id: &'a str,
    pub template_id: &'a str,
    pub team_id: &'a str,
    pub firecracker_version: &'a str,
}

pub type ResumedLayer = SetUseMemfd<SetPrefetchSettings<SetPrefetch<SetMemfile<SetRootfs<BaseSandbox>>>>>;

impl Node {
    pub async fn layer_source(&self, build_id: &str) -> Result<LayerSource, String> {
        let id = Uuid::parse_str(build_id).map_err(|e| format!("failed to parse build id: {e}"))?;
        let template = self.templates.get(id);
        let metadata = template.metadata().await.map_err(|e| e.to_string())?;
        let snapfile = match metadata.filesystem_only {
            true => None,
            false => Some(template.snapfile().await.map_err(|e| e.to_string())?),
        };
        let build = template.build().await.map_err(|e| e.to_string())?;
        Ok(LayerSource {
            template,
            build,
            snapfile,
            metadata,
        })
    }

    pub fn scratch_dir(&self) -> &Path {
        self.store.cache_dir()
    }

    pub fn rootfs_file(&self, header: Header) -> Arc<BuildFile> {
        Arc::new(BuildFile::new(header, Artifact::Rootfs, self.store.clone()))
    }

    pub fn cache_filesystem_layer(
        &self,
        layer: &FilesystemSnapshot,
        metadata: &TemplateMetadata,
        dir: PathBuf,
    ) -> Result<(), String> {
        let build_id = layer.build.build_id;
        if let Some(diff) = &layer.rootfs_diff {
            self.store.add_local(build_id, Artifact::Rootfs, diff)?;
        }
        self.templates
            .add_filesystem_layer(layer.build.clone(), metadata.clone(), dir);
        Ok(())
    }

    pub fn spawn_filesystem_upload(
        &self,
        layer: FilesystemSnapshot,
        object_metadata: ObjectMetadata,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        let build = layer.build.clone();
        let attempt = filesystem_upload_attempt(layer, self.storage.clone(), object_metadata);
        let uploads = self.uploads.clone();
        let store = self.store.clone();
        let task = tokio::spawn(async move {
            let uploaded = uploads.run(build.build_id, parents(&build), attempt).await;
            store.unpin(build.build_id);
            uploaded
        });
        async move { task.await.map_err(|e| format!("snapshot upload task failed: {e}"))? }
    }

    pub async fn put_layer_metadata(
        &self,
        source: &LayerSource,
        metadata: &TemplateMetadata,
        object_metadata: &ObjectMetadata,
    ) -> Result<(), String> {
        self.uploads.wait(source.build.build_id).await;
        self.storage
            .put_with(
                &source.build.paths().metadata(),
                Bytes::from(metadata.to_json()),
                object_metadata,
            )
            .await
            .map_err(|e| format!("failed to write metadata: {e}"))?;
        source.template.update_metadata(metadata.clone());
        Ok(())
    }

    pub fn resume_layer(&self, layer: &LayerSandbox) -> Result<SandboxBuilder<ResumedLayer>, String> {
        let work_dir = self.layer_work_dir(layer.sandbox_id)?;
        let memfile = Arc::new(BuildFile::new(
            layer.source.build.memfile.clone(),
            Artifact::Memfile,
            self.store.clone(),
        ));
        Ok(self
            .build_sandbox(layer.sandbox_id, layer.template_id)
            .rootfs(self.layer_overlay(&layer.source.build, &work_dir)?)
            .memfile(memfile)
            .maybe_prefetch(layer.source.metadata.memory_prefetch().cloned())
            .prefetch_settings(self.layer_prefetch_settings(layer))
            .use_memfd(
                supports_memfd(layer.firecracker_version)
                    && self.runtime_flags.use_memfd(FlagContext::sandbox_runtime(
                        layer.sandbox_id,
                        layer.template_id,
                        &layer.source.metadata.template.kernel_version,
                        layer.firecracker_version,
                        "",
                        BUILD_SANDBOX_TYPE,
                    )),
            )
            .envd(self.build_envd(EnvdDefaults {
                user: super::default_user(Some(&layer.source.metadata)),
                workdir: None,
            })))
    }

    pub fn build_envd(&self, defaults: EnvdDefaults) -> EnvdInit {
        EnvdInit {
            default_user: defaults.user,
            default_workdir: defaults.workdir,
            lifecycle_id: Some(Uuid::new_v4().to_string()),
            hyperloop_ip: Some(self.orchestrator_ip.clone()),
            request_timeout: Some(self.runtime_flags.envd_init_request_timeout()),
            ..EnvdInit::default()
        }
    }

    pub fn cold_boot_layer(
        &self,
        layer: &LayerSandbox,
    ) -> Result<SandboxBuilder<SetMemoryBase<SetRootfs<BaseSandbox>>>, String> {
        let work_dir = self.layer_work_dir(layer.sandbox_id)?;
        Ok(self
            .build_sandbox(layer.sandbox_id, layer.template_id)
            .rootfs(self.layer_overlay(&layer.source.build, &work_dir)?)
            .memory_base(layer.source.build.build_id))
    }

    fn layer_work_dir(&self, sandbox_id: &str) -> Result<PathBuf, String> {
        let work_dir = self.work_dir.join(sandbox_id);
        std::fs::create_dir_all(&work_dir)
            .map(|()| work_dir.clone())
            .map_err(|e| format!("create {}: {e}", work_dir.display()))
    }

    fn layer_overlay(&self, build: &Build, work_dir: &Path) -> Result<Arc<Overlay>, String> {
        let rootfs = Arc::new(BuildFile::new(
            build.rootfs.clone(),
            Artifact::Rootfs,
            self.store.clone(),
        ));
        Overlay::new(rootfs, &work_dir.join("rootfs.overlay"))
            .map(Arc::new)
            .map_err(|e| format!("create rootfs overlay: {e}"))
    }
}

async fn filesystem_upload_attempt(
    layer: FilesystemSnapshot,
    storage: Storage,
    object_metadata: ObjectMetadata,
) -> Result<(), Failure> {
    if !layer.exist() {
        return Err(Failure::Terminal("snapshot files are gone".to_owned()));
    }
    layer
        .upload(&storage, &object_metadata)
        .await
        .map_err(Failure::Retryable)
}
