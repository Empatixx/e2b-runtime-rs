use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::options::PauseOptions;
use super::report::fmt_dur;
use super::runner::Runner;
use crate::memory::trace::Trace;
use crate::server::lifecycle::Command;
use crate::storage::paths::BuildPaths;
use crate::template::metadata::{PREFETCH_ORIGIN_BUILD, TemplateMetadata};
use crate::template::optimize::PREFETCH_ITERATIONS;

const COLLECTION_LIFETIME: Duration = Duration::from_secs(5 * 60);

pub async fn collect_prefetch(runner: &Runner, options: &PauseOptions) -> Result<(), String> {
    println!("\n🔍 Collecting prefetch mapping...");
    let build_id = Uuid::parse_str(&options.new_build_id).map_err(|e| format!("load template: {e}"))?;
    runner.node.templates.invalidate(build_id);
    runner.drop_prefetch(&options.new_build_id).await;
    let mut traces = Vec::with_capacity(PREFETCH_ITERATIONS);
    for run in 1..=PREFETCH_ITERATIONS {
        print!("   Run {run}/{PREFETCH_ITERATIONS}...");
        traces.push(trace_once(runner, &options.new_build_id, run).await?);
    }
    let Some(common) = Trace::intersect(&traces) else {
        println!("⚠️  No common prefetch blocks found");
        return Ok(());
    };
    let mapping = common.to_mapping();
    println!("   Common: {} blocks", mapping.indices.len());
    let object = BuildPaths::new(build_id).metadata();
    let stored = runner
        .node
        .storage
        .find(&object)
        .await
        .map_err(|e| format!("load metadata: {e}"))?
        .ok_or_else(|| format!("load metadata: {object} not found"))?;
    let metadata = TemplateMetadata::from_json(&stored).map_err(|e| format!("load metadata: {e}"))?;
    let count = mapping.indices.len();
    let updated = metadata.with_prefetch(Some(mapping), PREFETCH_ORIGIN_BUILD);
    runner
        .node
        .storage
        .put(&object, Bytes::from(updated.to_json()))
        .await
        .map_err(|e| format!("upload metadata: {e}"))?;
    runner.node.templates.invalidate(build_id);
    println!("✅ Prefetch mapping saved: {count} blocks");
    Ok(())
}

async fn trace_once(runner: &Runner, build_id: &str, run: usize) -> Result<Trace, String> {
    let started_at = Instant::now();
    let id = format!("prefetch-{}-{}", now_nanos(), run - 1);
    let started = runner
        .start_from(build_id, id, COLLECTION_LIFETIME)
        .await
        .inspect_err(|_| println!())
        .map_err(|e| format!("resume sandbox (run {run}): {e}"))?;
    let trace = memory_trace(runner, &started.id).await;
    runner.close(&started.id).await;
    let trace = trace.ok_or_else(|| format!("collect prefetch (run {run}): no memory trace"))?;
    println!(" {} blocks ({})", trace.touches.len(), fmt_dur(started_at.elapsed()));
    Ok(trace)
}

async fn memory_trace(runner: &Runner, id: &str) -> Option<Trace> {
    let handle = runner.node.registry.get(id)?;
    let (reply, trace) = oneshot::channel();
    handle.commands.send(Command::Trace { reply }).await.ok()?;
    trace.await.ok().flatten()
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}
