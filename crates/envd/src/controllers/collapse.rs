use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::time::Instant;

use crate::host::collapse::{CollapseStats, collapse_self};
use crate::util::error_response::ErrorResponse;
use crate::util::go_json;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollapseResult {
    already_huge: usize,
    chunks: usize,
    collapsed: usize,
    elapsed_ms: i64,
    regions: usize,
    skipped: usize,
}

pub async fn post_collapse() -> Result<Response, ErrorResponse> {
    let started = Instant::now();
    let collapsed = tokio::task::spawn_blocking(collapse_self)
        .await
        .map_err(|e| e.to_string())
        .and_then(|collapsed| collapsed);
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let stats = collapsed.map_err(|e| {
        tracing::error!(error = %e, elapsed_ms, "collapse envd heap");
        ErrorResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .message(e)
            .build()
    })?;
    log(&stats, elapsed_ms);
    let body = go_json::encode(&CollapseResult {
        already_huge: stats.already_huge,
        chunks: stats.chunks,
        collapsed: stats.collapsed,
        elapsed_ms,
        regions: stats.regions,
        skipped: stats.skipped,
    })
    .unwrap_or_default();
    Ok((
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        body,
    )
        .into_response())
}

fn log(stats: &CollapseStats, elapsed_ms: i64) {
    tracing::info!(
        regions = stats.regions,
        chunks = stats.chunks,
        collapsed = stats.collapsed,
        already_huge = stats.already_huge,
        skipped = stats.skipped,
        elapsed_ms,
        "collapsed envd heap"
    );
}
