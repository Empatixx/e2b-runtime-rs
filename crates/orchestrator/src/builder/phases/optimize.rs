use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;

use super::steps::StepsBuild;
use crate::builder::commands::REQUEST_CANCELLED;
use crate::builder::hash::optimize_hash;
use crate::builder::layer::{BUILD_ORIGIN, LayerExecutor};
use crate::builder::logs::ScopedLog;
use crate::builder::phase::{BuildError, LayerResult, Phase, PhaseMeta};
use crate::memory::trace::Trace;
use crate::server::node::layers::LayerSource;
use crate::template::metadata::{MemoryPrefetch, PREFETCH_ORIGIN_BUILD, TemplateMetadata};
use crate::template::optimize::collect_prefetch;

const OPTIMIZE: &str = "optimize";
const OPTIMIZE_TEXT: &str = "Optimizing template";

pub struct OptimizePhase {
    pub build: Arc<StepsBuild>,
}

impl OptimizePhase {
    async fn optimize(
        &self,
        log: &ScopedLog<'_>,
        source: &LayerResult,
        current: LayerResult,
    ) -> Result<LayerResult, BuildError> {
        tracing::info!("Collecting prefetch mapping from template resume");
        let layer = self
            .build
            .env
            .node
            .layer_source(&source.metadata.template.build_id)
            .await
            .map_err(|e| BuildError::Internal(format!("failed to get template from cache: {e}")))?;
        let executor = self.build.executor(log);
        let metadata = match self.collect(&executor, &layer).await {
            Ok(mapping) => self.with_prefetch(&executor, &layer, &source.metadata, mapping).await,
            Err(e) => {
                tracing::warn!(error = %e, "failed to collect prefetch mapping, continuing without prefetch");
                source.metadata.clone()
            }
        };
        Ok(LayerResult { metadata, ..current })
    }

    async fn collect(
        &self,
        executor: &LayerExecutor<'_>,
        layer: &LayerSource,
    ) -> Result<Option<MemoryPrefetch>, String> {
        tokio::select! {
            biased;
            () = self.build.cancel.cancelled() => Err(REQUEST_CANCELLED.to_owned()),
            collected = collect_prefetch(|_| prefetch_run(executor, layer)) => collected,
        }
    }

    async fn with_prefetch(
        &self,
        executor: &LayerExecutor<'_>,
        layer: &LayerSource,
        source: &TemplateMetadata,
        mapping: Option<MemoryPrefetch>,
    ) -> TemplateMetadata {
        let block_count = mapping.as_ref().map_or(0, |mapping| mapping.indices.len());
        let updated = source.with_prefetch(mapping, PREFETCH_ORIGIN_BUILD);
        let object_metadata = executor.object_metadata(BUILD_ORIGIN);
        match self
            .build
            .env
            .node
            .put_layer_metadata(layer, &updated, &object_metadata)
            .await
        {
            Ok(()) => {
                tracing::info!(block_count, "Collected prefetch mapping with memory blocks");
                updated
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to upload prefetch metadata, continuing without prefetch");
                source.clone()
            }
        }
    }
}

async fn prefetch_run(executor: &LayerExecutor<'_>, layer: &LayerSource) -> Result<Trace, String> {
    let sandbox = executor
        .resume_source(layer)
        .await
        .map_err(|e| format!("failed to resume sandbox: {}", e.detail()))?;
    let trace = sandbox
        .memory_trace()
        .ok_or_else(|| "failed to get prefetch data: failed to get prefetch data: memory is not tracked".to_owned());
    let _ = sandbox.stop().await;
    trace
}

impl Phase for OptimizePhase {
    fn prefix(&self) -> String {
        OPTIMIZE.to_owned()
    }

    fn describe(&self) -> String {
        OPTIMIZE_TEXT.to_owned()
    }

    fn meta(&self) -> PhaseMeta {
        PhaseMeta {
            phase: OPTIMIZE,
            step_type: OPTIMIZE.to_owned(),
            step_number: None,
        }
    }

    fn hash<'a>(&'a self, source: &'a LayerResult) -> BoxFuture<'a, Result<String, BuildError>> {
        let hash = optimize_hash(&source.hash);
        async move { Ok(hash) }.boxed()
    }

    fn layer<'a>(&'a self, source: &'a LayerResult, hash: String) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        let layer = LayerResult {
            metadata: source.metadata.clone(),
            cached: false,
            hash,
        };
        async move { Ok(layer) }.boxed()
    }

    fn build<'a>(
        &'a self,
        log: ScopedLog<'a>,
        source: &'a LayerResult,
        current: LayerResult,
    ) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move { self.optimize(&log, source, current).await }.boxed()
    }
}
