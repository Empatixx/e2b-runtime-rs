use std::net::SocketAddr;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::BuildEnv;
use crate::builder::commands::{self, CommandRun};
use crate::builder::hash::{step_hash, user_hash};
use crate::builder::index::{CacheMiss, LayerIndex};
use crate::builder::layer::{
    BUILD_CACHE_ORIGIN, LayerBuild, LayerExecutor, SandboxCreator, SandboxShape, sync_changes_to_disk,
};
use crate::builder::logs::ScopedLog;
use crate::builder::phase::{BuildError, LayerResult, Phase, PhaseMeta, internal_context};
use crate::builder::uploads::LayerUploads;
use crate::envd::EnvdDefaults;
use crate::proto::{LogLevel, TemplateStep};
use crate::template::metadata::{TemplateMetadata, TemplateVersions};

const STEPS_PHASE: &str = "steps";
const BASE_PHASE: &str = "base";
const DEFAULT_USER: &str = "user";
const DEFAULT_USER_TEXT: &str = "DEFAULT USER";
const DEFAULT_USER_STEP_NUMBER: usize = 1;

pub struct StepsBuild {
    pub env: Arc<BuildEnv>,
    pub index: Arc<LayerIndex>,
    pub uploads: Arc<LayerUploads>,
    pub template_id: String,
    pub team_id: String,
    pub cache_scope: String,
    pub vcpus: i64,
    pub memory_mb: i64,
    pub envd_version: String,
    pub cancel: CancellationToken,
}

impl StepsBuild {
    pub fn executor<'a>(&'a self, log: &'a ScopedLog<'a>) -> LayerExecutor<'a> {
        LayerExecutor {
            env: &self.env,
            index: &self.index,
            uploads: &self.uploads,
            log,
            template_id: &self.template_id,
            team_id: &self.team_id,
        }
    }

    pub fn shape(&self) -> SandboxShape<'_> {
        SandboxShape {
            vcpus: self.vcpus,
            memory_mb: self.memory_mb,
            envd_version: &self.envd_version,
        }
    }
}

pub enum StepRole {
    Step { number: usize, total: usize },
    DefaultUser,
}

pub struct StepPhase {
    pub build: Arc<StepsBuild>,
    pub step: TemplateStep,
    pub role: StepRole,
}

impl StepPhase {
    pub fn default_user(build: Arc<StepsBuild>, force: Option<bool>) -> Self {
        Self {
            build,
            step: TemplateStep {
                r#type: "USER".to_owned(),
                args: vec![DEFAULT_USER.to_owned(), "true".to_owned()],
                force,
                files_hash: None,
            },
            role: StepRole::DefaultUser,
        }
    }

    fn number(&self) -> usize {
        match self.role {
            StepRole::Step { number, .. } => number,
            StepRole::DefaultUser => DEFAULT_USER_STEP_NUMBER,
        }
    }

    fn level(&self) -> LogLevel {
        match self.role {
            StepRole::Step { .. } => LogLevel::Info,
            StepRole::DefaultUser => LogLevel::Debug,
        }
    }

    fn error_meta(&self) -> PhaseMeta {
        PhaseMeta {
            phase: STEPS_PHASE,
            step_type: self.step.r#type.clone(),
            step_number: Some(self.number()),
        }
    }

    fn fresh_layer(&self, source: &LayerResult, hash: String) -> LayerResult {
        let config = &self.build.env.config;
        let metadata = TemplateMetadata {
            template: TemplateVersions {
                build_id: Uuid::new_v4().to_string(),
                kernel_version: config.kernel_version.clone(),
                firecracker_version: config.firecracker_version.clone(),
            },
            ..source.metadata.clone()
        };
        LayerResult {
            metadata,
            cached: false,
            hash,
        }
    }

    async fn cached_layer(&self, source: &LayerResult, hash: String) -> LayerResult {
        if self.step.force.unwrap_or(false) {
            return self.fresh_layer(source, hash);
        }
        match self.build.index.cached(&hash).await {
            Ok(layer) => LayerResult {
                metadata: layer.metadata,
                cached: true,
                hash,
            },
            Err(CacheMiss::NoLayer(e)) => {
                tracing::info!(error = %e, hash = %hash, "layer not found in cache, building new step layer");
                self.fresh_layer(source, hash)
            }
            Err(CacheMiss::NoMetadata(e)) => {
                tracing::info!(error = %e, hash = %hash, "layer not cached, building new layer");
                self.fresh_layer(source, hash)
            }
        }
    }

    async fn build_step(
        &self,
        log: &ScopedLog<'_>,
        source: &LayerResult,
        current: LayerResult,
    ) -> Result<LayerResult, BuildError> {
        let creator = match source.cached {
            true => SandboxCreator::ColdBoot,
            false => SandboxCreator::Resume,
        };
        let layer = LayerBuild {
            source_build_id: &source.metadata.template.build_id,
            current: current.metadata,
            hash: &current.hash,
            update_envd: source.cached,
            creator,
            shape: self.build.shape(),
            envd_defaults: EnvdDefaults::default(),
            reserved_blocks: source.cached,
            build_origin: BUILD_CACHE_ORIGIN,
        };
        let metadata = self
            .build
            .executor(log)
            .build_layer(layer, |envd, metadata| self.execute(log, envd, metadata))
            .await
            .map_err(|e| internal_context(&format!("error building step {}", self.number()), e))?;
        Ok(LayerResult {
            metadata,
            cached: false,
            hash: current.hash,
        })
    }

    async fn execute(
        &self,
        log: &ScopedLog<'_>,
        envd: SocketAddr,
        mut metadata: TemplateMetadata,
    ) -> Result<TemplateMetadata, BuildError> {
        let prefix = self.prefix();
        let run = CommandRun {
            envd,
            log,
            level: self.level(),
            prefix: &prefix,
            files: &self.build.env.build_storage,
            cache_scope: &self.build.cache_scope,
            cancel: &self.build.cancel,
        };
        let context = std::mem::take(&mut metadata.context);
        let context = commands::execute(&run, &self.step, context)
            .await
            .map_err(|e| BuildError::user(&self.error_meta(), e))?;
        sync_changes_to_disk(envd)
            .await
            .map_err(|e| BuildError::Internal(format!("error running sync command: {e}")))?;
        metadata.context = context;
        Ok(metadata)
    }
}

impl Phase for StepPhase {
    fn prefix(&self) -> String {
        match self.role {
            StepRole::Step { number, total } => format!("builder {number}/{total}"),
            StepRole::DefaultUser => BASE_PHASE.to_owned(),
        }
    }

    fn describe(&self) -> String {
        match self.role {
            StepRole::Step { .. } => step_text(&self.step),
            StepRole::DefaultUser => format!("{DEFAULT_USER_TEXT} {DEFAULT_USER}"),
        }
    }

    fn meta(&self) -> PhaseMeta {
        match self.role {
            StepRole::Step { .. } => self.error_meta(),
            StepRole::DefaultUser => PhaseMeta {
                phase: BASE_PHASE,
                step_type: BASE_PHASE.to_owned(),
                step_number: None,
            },
        }
    }

    fn hash<'a>(&'a self, source: &'a LayerResult) -> BoxFuture<'a, Result<String, BuildError>> {
        let hash = match self.role {
            StepRole::Step { .. } => step_hash(
                &source.hash,
                &self.step.r#type,
                &self.step.args,
                self.step.files_hash.as_deref(),
            ),
            StepRole::DefaultUser => user_hash(&source.hash),
        };
        async move { Ok(hash) }.boxed()
    }

    fn layer<'a>(&'a self, source: &'a LayerResult, hash: String) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move { Ok(self.cached_layer(source, hash).await) }.boxed()
    }

    fn build<'a>(
        &'a self,
        log: ScopedLog<'a>,
        source: &'a LayerResult,
        current: LayerResult,
    ) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move { self.build_step(&log, source, current).await }.boxed()
    }
}

fn step_text(step: &TemplateStep) -> String {
    format!("{} {}", step.r#type.to_uppercase(), step.args.join(" "))
}

pub fn force_steps(force: bool, steps: &[TemplateStep]) -> Vec<TemplateStep> {
    steps
        .iter()
        .scan(force, |rebuild, step| {
            *rebuild |= step.force.unwrap_or(false);
            Some(TemplateStep {
                force: rebuild.then_some(true).or(step.force),
                ..step.clone()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(force: Option<bool>) -> TemplateStep {
        TemplateStep {
            r#type: "RUN".to_owned(),
            force,
            ..TemplateStep::default()
        }
    }

    #[test]
    fn a_step_reads_as_its_upper_cased_type_and_args() {
        let run = TemplateStep {
            r#type: "run".to_owned(),
            args: vec!["echo hi".to_owned(), "root".to_owned()],
            ..TemplateStep::default()
        };

        assert_eq!(step_text(&run), "RUN echo hi root");
        assert_eq!(step_text(&step(None)), "RUN ");
    }

    #[test]
    fn force_is_sticky_from_the_first_forced_step() {
        let forced: Vec<Option<bool>> =
            force_steps(false, &[step(None), step(Some(false)), step(Some(true)), step(None)])
                .into_iter()
                .map(|step| step.force)
                .collect();

        assert_eq!(forced, vec![None, Some(false), Some(true), Some(true)]);
        assert!(
            force_steps(true, &[step(None)])
                .iter()
                .all(|step| step.force == Some(true))
        );
    }
}
