pub mod adapters;
pub mod after_pause;
pub mod harvester;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use uuid::Uuid;

use crate::proto::SandboxConfig;
use crate::storage::backend::ObjectMetadata;
use crate::telemetry::server::prefetch as telemetry;
use crate::template::Template;
use crate::template::metadata::{MemoryPrefetch, TemplateMetadata};

pub const MIN_HARVEST_TIMEOUT_MS: i64 = 1000;
pub const HARVEST_REAP_TIMEOUT: Duration = Duration::from_secs(60);
pub const UPLOAD_TOTAL_BUDGET: Duration = Duration::from_secs(2 * 60 * 60);
pub const THROWAWAY_ID_PREFIX: &str = "prefetch-harvest-";
pub const DEADLINE_EXCEEDED: &str = "context deadline exceeded";

pub fn clamp_harvest_timeout_ms(ms: i64) -> i64 {
    ms.max(MIN_HARVEST_TIMEOUT_MS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarvestOutcome {
    Success,
    ResumeFailed,
    CollectFailed,
    Skipped,
    PersistDeadline,
}

impl HarvestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ResumeFailed => "resume_failed",
            Self::CollectFailed => "collect_failed",
            Self::Skipped => "skipped",
            Self::PersistDeadline => "persist_deadline",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarvestRun {
    pub pages: usize,
    pub outcome: HarvestOutcome,
    pub slot_hold: Duration,
    pub persist_wait: Duration,
    pub persist_attempted: bool,
}

impl HarvestRun {
    pub fn failed(outcome: HarvestOutcome, slot_hold: Duration) -> Self {
        Self {
            pages: 0,
            outcome,
            slot_hold,
            persist_wait: Duration::ZERO,
            persist_attempted: false,
        }
    }

    pub fn record_metrics(&self) {
        telemetry::HarvestRun {
            result: self.outcome.as_str().to_owned(),
            slot_hold: self.slot_hold,
            persist_wait: self.persist_attempted.then_some(self.persist_wait),
            pages: matches!(self.outcome, HarvestOutcome::Success).then_some(self.pages as u64),
        }
        .record();
    }

    pub fn succeeded(pages: usize, slot_hold: Duration) -> Self {
        Self {
            outcome: HarvestOutcome::Success,
            pages,
            ..Self::failed(HarvestOutcome::Success, slot_hold)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarvestReport {
    pub run: HarvestRun,
    pub error: Option<String>,
}

impl HarvestReport {
    pub fn ok(run: HarvestRun) -> Self {
        Self { run, error: None }
    }

    pub fn failed(run: HarvestRun, error: String) -> Self {
        Self {
            run,
            error: Some(error),
        }
    }
}

pub struct HarvestFailure {
    pub outcome: HarvestOutcome,
    pub message: String,
}

impl HarvestFailure {
    pub fn new(outcome: HarvestOutcome, message: impl Into<String>) -> Self {
        Self {
            outcome,
            message: message.into(),
        }
    }
}

pub struct ThrowawayResume {
    pub template: Arc<Template>,
    pub config: SandboxConfig,
    pub start: SystemTime,
    pub end: SystemTime,
}

#[async_trait]
pub trait HarvestInstance: Send + Sync {
    async fn memory_prefetch(&self) -> Result<Option<MemoryPrefetch>, String>;
    async fn stop(&self) -> Result<(), String>;
    async fn close(&self) -> Result<(), String>;
}

#[async_trait]
pub trait HarvestResumer: Send + Sync {
    async fn resume_for_harvest(&self, resume: ThrowawayResume) -> Result<Box<dyn HarvestInstance>, String>;
}

pub type StartSlot = Box<dyn std::any::Any + Send + Sync>;

#[async_trait]
pub trait HarvestSlots: Send + Sync {
    async fn acquire(&self) -> Result<StartSlot, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataWriteError {
    pub message: String,
    pub committed: bool,
}

impl MetadataWriteError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            committed: false,
        }
    }

    pub fn committed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            committed: true,
        }
    }
}

#[async_trait]
pub trait HarvestTemplates: Send + Sync {
    async fn pin(&self, build_id: Uuid, max_sandbox_length_hours: i64) -> Result<Arc<Template>, String>;
    async fn update_metadata(&self, build_id: Uuid, metadata: &TemplateMetadata) -> Result<(), MetadataWriteError>;
}

#[async_trait]
pub trait HarvestUpload: Send + Sync {
    async fn wait(&self) -> Result<(), String>;
}

#[async_trait]
pub trait MetadataUploader: Send + Sync {
    async fn upload_metadata(
        &self,
        build_id: Uuid,
        metadata: &TemplateMetadata,
        object_metadata: &ObjectMetadata,
    ) -> Result<(), String>;
}
