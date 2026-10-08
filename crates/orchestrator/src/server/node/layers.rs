use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use super::Node;
use crate::block::build_file::{Artifact, BuildFile};
use crate::block::overlay::Overlay;
use crate::envd::{EnvdDefaults, EnvdInit};
use crate::firecracker::version::supports_memfd;
use crate::sandbox::config::SandboxBuilder;
use crate::storage::build::Build;
use crate::template::Template;
use crate::template::metadata::TemplateMetadata;

pub struct LayerSource {
    pub template: Arc<Template>,
    pub build: Build,
    pub snapfile: PathBuf,
    pub metadata: TemplateMetadata,
}

pub struct LayerSandbox<'a> {
    pub source: &'a LayerSource,
    pub sandbox_id: &'a str,
    pub template_id: &'a str,
    pub firecracker_version: &'a str,
}

impl Node {
    pub async fn layer_source(&self, build_id: &str) -> Result<LayerSource, String> {
        let snapshot = |e: String| format!("get template snapshot: get template snapshot: {e}");
        let id = Uuid::parse_str(build_id).map_err(|e| snapshot(format!("failed to parse build id: {e}")))?;
        let template = self.templates.get(id);
        let (build, snapfile, metadata) = tokio::try_join!(template.build(), template.snapfile(), template.metadata())
            .map_err(|e| snapshot(e.to_string()))?;
        Ok(LayerSource {
            template,
            build,
            snapfile,
            metadata,
        })
    }

    pub fn resume_layer(&self, layer: &LayerSandbox) -> Result<SandboxBuilder, String> {
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
            .use_memfd(self.use_memfd && supports_memfd(layer.firecracker_version))
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
            ..EnvdInit::default()
        }
    }

    pub fn cold_boot_layer(&self, layer: &LayerSandbox) -> Result<SandboxBuilder, String> {
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
