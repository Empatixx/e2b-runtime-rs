use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::sync::oneshot;
use tracing::Instrument;
use uuid::Uuid;

use super::layers::LayerSandbox;
use super::{BUILD_SANDBOX_TYPE, Lifetime, Node, StartRequest};
use crate::featureflags::flag_context_builder::{SetKey, SetKind};
use crate::featureflags::{FlagContext, FlagContextBuilder};
use crate::memory::prefetch::source::PrefetchSettings;
use crate::memory::trace::Trace;
use crate::proto::SandboxConfig;
use crate::sandbox::harvest::adapters::{SnapshotUpload, StorageMetadataUploader};
use crate::sandbox::harvest::after_pause::{HarvestSettings, PauseHarvest, PrefetchHarvest};
use crate::sandbox::harvest::harvester::PrefetchHarvester;
use crate::sandbox::harvest::{
    HarvestInstance, HarvestResumer, HarvestSlots, StartSlot, THROWAWAY_ID_PREFIX, ThrowawayResume,
};
use crate::server::lifecycle::{Command, SandboxHandle, SandboxInfo};
use crate::server::oom_monitor::SharedWatermark;
use crate::server::registry::Lifecycle;
use crate::storage::backend::ObjectMetadata;

const SANDBOX_TYPE: &str = "sandbox";
const NO_TRACE: &str = "memory trace is not available";

pub struct PausedBuild {
    pub parent: SandboxInfo,
    pub build_id: Uuid,
    pub object_metadata: ObjectMetadata,
}

impl Node {
    pub(super) fn prefetch_settings(&self, config: &SandboxConfig) -> PrefetchSettings {
        let flags = self.feature_flags.scoped(resume_contexts(config));
        PrefetchSettings::from_flags(&flags)
    }

    pub(super) fn layer_prefetch_settings(&self, layer: &LayerSandbox) -> PrefetchSettings {
        let sandbox = FlagContext::sandbox_runtime(
            layer.sandbox_id,
            layer.template_id,
            &layer.source.metadata.template.kernel_version,
            layer.firecracker_version,
            "",
            BUILD_SANDBOX_TYPE,
        );
        let flags = self.feature_flags.scoped(vec![
            sandbox,
            FlagContext::template(layer.template_id),
            FlagContext::team(layer.team_id),
        ]);
        PrefetchSettings::from_flags(&flags)
    }

    pub fn harvest_after_pause(self: &Arc<Self>, paused: PausedBuild) {
        let flags = self.feature_flags.scoped(pause_contexts(&paused.parent.config));
        let Some(settings) = HarvestSettings::from_flags(&flags) else {
            return;
        };
        let build_id = paused.build_id;
        let parent = paused.parent;
        let pause = PauseHarvest {
            config: parent.config,
            build_id,
            start: parent.start,
            end: parent.end,
            template: self.templates.get(build_id),
            upload: Arc::new(SnapshotUpload {
                uploads: self.uploads.clone(),
                build_id,
            }),
            object_metadata: paused.object_metadata,
            work: Some(Box::new(self.status.track_work())),
        };
        PrefetchHarvest::new(self.harvester()).after_pause(settings, pause);
    }

    fn harvester(self: &Arc<Self>) -> Arc<PrefetchHarvester> {
        let host = Arc::new(ThrowawayHost { node: self.clone() });
        Arc::new(PrefetchHarvester::new(
            host.clone(),
            self.templates.clone(),
            host,
            Arc::new(StorageMetadataUploader {
                storage: self.storage.clone(),
            }),
        ))
    }
}

fn sandbox_attributes(config: &SandboxConfig) -> FlagContextBuilder<SetKey<SetKind>> {
    FlagContext::builder()
        .kind("sandbox")
        .key(&config.sandbox_id)
        .attribute("template-id", &config.template_id)
        .attribute("kernel-version", &config.kernel_version)
        .attribute("firecracker-version", &config.firecracker_version)
        .attribute("envd-version", &config.envd_version)
}

fn resume_contexts(config: &SandboxConfig) -> Vec<FlagContext> {
    vec![
        sandbox_attributes(config)
            .attribute("sandbox-type", SANDBOX_TYPE)
            .build(),
        FlagContext::team(&config.team_id),
    ]
}

pub(super) fn pause_contexts(config: &SandboxConfig) -> Vec<FlagContext> {
    vec![sandbox_attributes(config).build(), FlagContext::team(&config.team_id)]
}

struct ThrowawayHost {
    node: Arc<Node>,
}

#[async_trait]
impl HarvestSlots for ThrowawayHost {
    async fn acquire(&self) -> Result<StartSlot, String> {
        self.node
            .admission
            .acquire()
            .instrument(tracing::info_span!("wait-for-acquire"))
            .await
            .map(|permit| Box::new(permit) as StartSlot)
            .map_err(|status| format!("rpc error: code = ResourceExhausted desc = {}", status.message()))
    }
}

#[async_trait]
impl HarvestResumer for ThrowawayHost {
    async fn resume_for_harvest(&self, resume: ThrowawayResume) -> Result<Box<dyn HarvestInstance>, String> {
        let ThrowawayResume {
            template,
            config,
            start,
            end,
        } = resume;
        debug_assert!(config.sandbox_id.starts_with(THROWAWAY_ID_PREFIX));
        let lifecycle = Lifecycle {
            sandbox_id: config.sandbox_id.clone(),
            lifecycle_id: Uuid::new_v4().to_string(),
            execution_id: config.execution_id.clone(),
        };
        let work_dir = WorkDirGuard::new(self.node.work_dir_of(&lifecycle));
        let handle = self
            .node
            .start(StartRequest {
                template,
                config,
                lifetime: lifetime(start, end),
                lifecycle,
                oom_watermark: SharedWatermark::default(),
                throwaway: true,
            })
            .await
            .map_err(|failure| failure.message())?;
        work_dir.defuse();
        Ok(Box::new(Throwaway { handle }))
    }
}

struct WorkDirGuard {
    path: Option<PathBuf>,
}

impl WorkDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn defuse(mut self) {
        self.path = None;
    }
}

impl Drop for WorkDirGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn lifetime(start: SystemTime, end: SystemTime) -> Lifetime {
    Lifetime { start, end }
}

struct Throwaway {
    handle: SandboxHandle,
}

#[async_trait]
impl HarvestInstance for Throwaway {
    async fn memory_prefetch(&self) -> Result<Option<crate::template::metadata::MemoryPrefetch>, String> {
        let (reply, trace) = oneshot::channel();
        self.handle
            .commands
            .send(Command::Trace { reply })
            .await
            .map_err(|_| NO_TRACE.to_owned())?;
        trace
            .await
            .ok()
            .flatten()
            .as_ref()
            .map(Trace::to_mapping)
            .map(Some)
            .ok_or_else(|| NO_TRACE.to_owned())
    }

    async fn stop(&self) -> Result<(), String> {
        let _ = self.handle.commands.send(Command::Delete).await;
        self.handle.commands.closed().await;
        Ok(())
    }

    async fn close(&self) -> Result<(), String> {
        Ok(())
    }
}
