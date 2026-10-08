use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;

use super::builds::BuildInfo;
use super::index::LayerIndex;
use super::layer::envd_version;
use super::phase::{BuildError, LayerResult, Phase, run_phases};
use super::phases::BuildEnv;
use super::phases::base::{BasePhase, BaseRequest, BaseSource};
use super::service::TemplateBuilder;
use super::uploads::LayerUploads;
use uuid::Uuid;

use crate::proto::template_config::Source;
use crate::proto::{TemplateBuildMetadata, TemplateConfig, TemplateCreateRequest};
use crate::storage::paths::BuildPaths;

const MB: u64 = 1 << 20;

pub struct PhasePipeline {
    pub env: Arc<BuildEnv>,
}

impl PhasePipeline {
    fn phases(
        &self,
        request: &TemplateCreateRequest,
        envd_version: &str,
        uploads: &Arc<LayerUploads>,
    ) -> Result<Vec<Box<dyn Phase>>, BuildError> {
        let config = request.template.clone().unwrap_or_default();
        let scope = request
            .cache_scope
            .clone()
            .unwrap_or_else(|| config.template_id.clone());
        let index = Arc::new(LayerIndex::new(
            self.env.build_storage.clone(),
            self.env.node.storage.clone(),
            &scope,
        ));
        let base = BasePhase {
            env: self.env.clone(),
            request: base_request(&config, envd_version)?,
            index,
            uploads: uploads.clone(),
        };
        Ok(vec![Box::new(base)])
    }
}

impl TemplateBuilder for PhasePipeline {
    fn build(
        &self,
        request: TemplateCreateRequest,
        info: Arc<BuildInfo>,
    ) -> BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>> {
        let env = self.env.clone();
        let pipeline = PhasePipeline { env: env.clone() };
        async move {
            let envd_version = envd_version(&env.config.envd)
                .await
                .map_err(|e| BuildError::Internal(format!("error getting envd version: {e}")))?;
            let uploads = Arc::new(LayerUploads::new(info.clone()));
            let phases = pipeline.phases(&request, &envd_version, &uploads)?;
            let built = run_phases(&info.log, &phases).await;
            let landed = uploads.wait().await;
            let last = match (built, landed) {
                (Err(e), Err(upload)) => {
                    tracing::error!(error = %upload, "template layer upload wait failed");
                    return Err(e);
                }
                (Err(e), Ok(())) => return Err(e),
                (Ok(_), Err(upload)) => {
                    return Err(BuildError::Internal(format!(
                        "error waiting for layers upload: {upload}"
                    )));
                }
                (Ok(last), Ok(())) => last,
            };
            let rootfs_size = rootfs_size(&env, &last).await?;
            Ok(result_metadata(&env, rootfs_size, envd_version))
        }
        .boxed()
    }
}

fn base_request(config: &TemplateConfig, envd_version: &str) -> Result<BaseRequest, BuildError> {
    let source = match &config.source {
        Some(Source::FromImage(image)) => BaseSource::Image {
            image: image.clone(),
            registry: config.from_image_registry.clone(),
        },
        Some(Source::FromTemplate(template)) => BaseSource::Template {
            alias: template.alias.clone(),
            build_id: template.build_id.clone(),
        },
        None => return Err(BuildError::Internal("build has no base image or template".to_owned())),
    };
    Ok(BaseRequest {
        template_id: config.template_id.clone(),
        team_id: config.team_id.clone(),
        source,
        memory_mb: config.memory_mb.into(),
        vcpus: config.v_cpu_count.into(),
        disk_size_mb: config.disk_size_mb.into(),
        force: config.force.unwrap_or(false),
        envd_version: envd_version.to_owned(),
    })
}

async fn rootfs_size(env: &BuildEnv, last: &LayerResult) -> Result<u64, BuildError> {
    let build_id = Uuid::parse_str(&last.metadata.template.build_id)
        .map_err(|e| BuildError::Internal(format!("error getting rootfs size: {e}")))?;
    env.node
        .storage
        .load_header(&BuildPaths::new(build_id).rootfs_header())
        .await
        .map(|header| header.metadata.size)
        .map_err(|e| BuildError::Internal(format!("error getting rootfs size: {e}")))
}

fn result_metadata(env: &BuildEnv, rootfs_size: u64, envd_version: String) -> TemplateBuildMetadata {
    TemplateBuildMetadata {
        rootfs_size_key: (rootfs_size / MB) as i32,
        envd_version_key: envd_version,
        kernel_version: env.config.kernel_version.clone(),
        firecracker_version: env.config.firecracker_version.clone(),
        scheduling_metadata: None,
    }
}
