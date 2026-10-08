use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::builds::BuildInfo;
use super::index::LayerIndex;
use super::layer::envd_version;
use super::phase::{BuildError, LayerResult, Phase, run_phases};
use super::phases::BuildEnv;
use super::phases::base::{BasePhase, BaseRequest, BaseSource};
use super::phases::finalize::FinalizePhase;
use super::phases::optimize::OptimizePhase;
use super::phases::steps::{StepPhase, StepRole, StepsBuild, force_steps};
use super::service::TemplateBuilder;
use super::uploads::LayerUploads;
use uuid::Uuid;

use crate::envd::version::is_at_least;
use crate::proto::template_config::Source;
use crate::proto::{SchedulingMetadata, TemplateBuildMetadata, TemplateConfig, TemplateCreateRequest};
use crate::server::scheduling::from_headers;
use crate::storage::paths::BuildPaths;

const MB: u64 = 1 << 20;
const TEMPLATE_V2_RELEASE_VERSION: &str = "v2.1.0";

pub struct PhasePipeline {
    pub env: Arc<BuildEnv>,
}

impl PhasePipeline {
    fn phases(
        &self,
        request: &TemplateCreateRequest,
        envd_version: &str,
        uploads: &Arc<LayerUploads>,
        cancel: &CancellationToken,
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
            index: index.clone(),
            uploads: uploads.clone(),
        };
        let steps = Arc::new(StepsBuild {
            env: self.env.clone(),
            index,
            uploads: uploads.clone(),
            template_id: config.template_id.clone(),
            team_id: config.team_id.clone(),
            cache_scope: scope,
            vcpus: config.v_cpu_count.into(),
            memory_mb: config.memory_mb.into(),
            envd_version: envd_version.to_owned(),
            cancel: cancel.clone(),
        });
        let template_v2 = with_default_user(request.version.as_deref().unwrap_or_default())?;
        let mut phases: Vec<Box<dyn Phase>> = vec![Box::new(base)];
        if template_v2 {
            phases.push(Box::new(StepPhase::default_user(steps.clone(), config.force)));
        }
        phases.extend(step_phases(&steps, &config));
        phases.push(Box::new(FinalizePhase {
            build: steps.clone(),
            build_id: config.build_id.clone(),
            start_command: config.start_command.clone(),
            ready_command: config.ready_command.clone(),
            context_defaults: template_v2,
        }));
        phases.push(Box::new(OptimizePhase { build: steps }));
        Ok(phases)
    }
}

fn step_phases(build: &Arc<StepsBuild>, config: &TemplateConfig) -> Vec<Box<dyn Phase>> {
    let total = config.steps.len();
    force_steps(config.force.unwrap_or(false), &config.steps)
        .into_iter()
        .enumerate()
        .map(|(index, step)| {
            Box::new(StepPhase {
                build: build.clone(),
                step,
                role: StepRole::Step {
                    number: index + 1,
                    total,
                },
            }) as Box<dyn Phase>
        })
        .collect()
}

fn with_default_user(version: &str) -> Result<bool, BuildError> {
    is_at_least(version, TEMPLATE_V2_RELEASE_VERSION)
        .map_err(|e| BuildError::Internal(format!("error checking build version: {e}")))
}

impl TemplateBuilder for PhasePipeline {
    fn build(
        &self,
        request: TemplateCreateRequest,
        info: Arc<BuildInfo>,
        uploads: Arc<LayerUploads>,
    ) -> BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>> {
        let env = self.env.clone();
        let pipeline = PhasePipeline { env: env.clone() };
        async move {
            let envd_version = envd_version(&env.config.envd)
                .await
                .map_err(|e| BuildError::Internal(format!("error getting envd version: {e}")))?;
            let phases = pipeline.phases(&request, &envd_version, &uploads, &info.cancel)?;
            let last = run_phases(&info.log, &phases).await?;
            uploads
                .wait()
                .await
                .map_err(|e| BuildError::Internal(format!("error waiting for layers upload: {e}")))?;
            let rootfs_size = rootfs_size(&env, &last).await?;
            let scheduling = scheduling_metadata(&env, &last).await;
            Ok(result_metadata(&env, rootfs_size, envd_version, scheduling))
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
    let size = |e: String| BuildError::Internal(format!("error getting rootfs size: {e}"));
    let build_id = Uuid::parse_str(&last.metadata.template.build_id)
        .map_err(|e| size(format!("error opening rootfs header object: {e}")))?;
    let header = env
        .node
        .storage
        .load_header(&BuildPaths::new(build_id).rootfs_header())
        .await
        .map_err(|e| size(format!("error deserializing rootfs header: {e}")))?;
    tracing::info!(size = header.metadata.size, "rootfs size");
    Ok(header.metadata.size)
}

async fn scheduling_metadata(env: &BuildEnv, last: &LayerResult) -> Option<SchedulingMetadata> {
    let build_id = Uuid::parse_str(&last.metadata.template.build_id).ok()?;
    let build = env.node.templates.get(build_id).build().await.ok()?;
    Some(from_headers(
        build.rootfs.metadata.build_id,
        Some(&build.memfile),
        &build.rootfs,
        0,
    ))
}

fn result_metadata(
    env: &BuildEnv,
    rootfs_size: u64,
    envd_version: String,
    scheduling_metadata: Option<SchedulingMetadata>,
) -> TemplateBuildMetadata {
    TemplateBuildMetadata {
        rootfs_size_key: (rootfs_size / MB) as i32,
        envd_version_key: envd_version,
        kernel_version: env.config.kernel_version.clone(),
        firecracker_version: env.config.firecracker_version.clone(),
        scheduling_metadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_user_phase_starts_at_v2_1_0() {
        assert_eq!(with_default_user("v2.0.0"), Ok(false));
        assert_eq!(with_default_user("v2.1.0"), Ok(true));
        assert_eq!(
            with_default_user("latest"),
            Err(BuildError::Internal(
                "error checking build version: invalid current version format: vlatest".to_owned()
            ))
        );
    }
}
