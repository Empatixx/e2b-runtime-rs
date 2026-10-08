use std::path::Path;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;

use super::builds::BuildInfo;
use super::index::LayerIndex;
use super::phase::{BuildError, LayerResult, Phase, run_phases};
use super::phases::BuildEnv;
use super::phases::base::{BasePhase, BaseRequest, BaseSource};
use super::service::TemplateBuilder;
use uuid::Uuid;

use crate::go_exec::GoCommand;
use crate::proto::template_config::Source;
use crate::proto::{TemplateBuildMetadata, TemplateConfig, TemplateCreateRequest};
use crate::storage::paths::BuildPaths;

const MB: u64 = 1 << 20;

pub struct PhasePipeline {
    pub env: Arc<BuildEnv>,
}

impl PhasePipeline {
    fn phases(&self, request: &TemplateCreateRequest, envd_version: &str) -> Result<Vec<Box<dyn Phase>>, BuildError> {
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
            let phases = pipeline.phases(&request, &envd_version)?;
            let last = run_phases(&info.log, &phases).await?;
            let rootfs_size = rootfs_size(&env, &last).await?;
            Ok(result_metadata(&last, rootfs_size, envd_version))
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

fn result_metadata(last: &LayerResult, rootfs_size: u64, envd_version: String) -> TemplateBuildMetadata {
    TemplateBuildMetadata {
        rootfs_size_key: (rootfs_size / MB) as i32,
        envd_version_key: envd_version,
        kernel_version: last.metadata.template.kernel_version.clone(),
        firecracker_version: last.metadata.template.firecracker_version.clone(),
        scheduling_metadata: None,
    }
}

async fn envd_version(envd: &Path) -> Result<String, String> {
    GoCommand::new(envd)
        .args(["-version"])
        .run()
        .await
        .map(|output| String::from_utf8_lossy(&output).trim().to_owned())
        .map_err(|e| format!("error while getting envd version: {}", e.message))
}
