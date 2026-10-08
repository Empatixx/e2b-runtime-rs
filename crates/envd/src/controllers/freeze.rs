use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::app::AppState;
use crate::cgroups::freeze::{FreezeMode, FreezeOptions, FreezeResult};
use crate::util::error_response::ErrorResponse;
use crate::util::go_json;
use crate::util::query::parse_int;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FreezeResultBody {
    allowlisted: i64,
    failed: i64,
    frozen: i64,
    mode: &'static str,
    not_frozen: i64,
    pre_frozen: i64,
    requested: i64,
    sweep_ms: i64,
    truncated: bool,
    unobservable: i64,
    vanished: i64,
    visited: i64,
    wait_ms: i64,
}

pub async fn post_freeze(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ErrorResponse> {
    let max_cgroups = query
        .get("maxCgroups")
        .map(|value| parse_int("maxCgroups", value))
        .transpose()?;
    let max_wait_ms = query
        .get("maxWaitMs")
        .map(|value| parse_int("maxWaitMs", value))
        .transpose()?;
    let report = max_wait_ms.is_some_and(|wait| wait > 0);
    let options = FreezeOptions {
        max_wait: Duration::from_millis(max_wait_ms.filter(|_| report).unwrap_or_default() as u64),
        mode: match query.get("mode").map(String::as_str) {
            Some("hierarchy") => FreezeMode::Hierarchy,
            _ => FreezeMode::Legacy,
        },
        max_cgroups: max_cgroups.unwrap_or_default(),
    };
    let freezer = state.freezer.clone();
    let (result, errors) = tokio::spawn(async move { freezer.freeze(options).await })
        .await
        .map_err(|e| {
            ErrorResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .message(e)
                .build()
        })?;
    if !errors.is_empty() {
        tracing::error!(error = %errors.join("\n"), failed = result.failed, "some cgroups failed to freeze; reporting them in the result");
    }
    if !report {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    log_observations(&result);
    let body = go_json::encode(&FreezeResultBody::from(&result)).unwrap_or_default();
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

pub async fn post_unfreeze(State(state): State<Arc<AppState>>) -> Result<Response, ErrorResponse> {
    let freezer = state.freezer.clone();
    let thawed = tokio::spawn(async move { freezer.unfreeze().await })
        .await
        .map_err(|e| e.to_string())
        .and_then(|thawed| thawed);
    match thawed {
        Ok(_) => Ok((StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")]).into_response()),
        Err(e) => {
            tracing::error!(error = %e, "unfreeze workload cgroups");
            Err(ErrorResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .message(e)
                .build())
        }
    }
}

fn log_observations(result: &FreezeResult) {
    if !result.all_frozen() {
        tracing::warn!(
            requested = result.requested,
            frozen = result.frozen,
            not_frozen = result.not_frozen,
            failed = result.failed,
            vanished = result.vanished,
            wait_duration = ?result.wait,
            "pre-pause freeze did not stop the whole workload; snapshot may capture it running"
        );
    }
    if result.vanished > 0 {
        tracing::debug!(
            vanished = result.vanished,
            requested = result.requested,
            frozen = result.frozen,
            vanished_paths = ?result.vanished_paths,
            "cgroups vanished while the freeze was working on them; counted, not failed"
        );
    }
    if result.scan_failed > 0 {
        tracing::warn!(
            scan_failed = result.scan_failed,
            pre_frozen = result.pre_frozen,
            "could not classify some cgroups as the guest's own; those will be thawed on resume"
        );
    }
    if result.truncated {
        tracing::warn!(
            visited = result.visited,
            requested = result.requested,
            "freeze walk hit its bound; some cgroups were never examined"
        );
    }
    if result.unobservable > 0 {
        tracing::warn!(
            unobservable = result.unobservable,
            "cgroup freeze state is unobservable on this guest; freeze issued but unverifiable"
        );
    }
}

impl From<&FreezeResult> for FreezeResultBody {
    fn from(result: &FreezeResult) -> Self {
        Self {
            allowlisted: result.allowlisted,
            failed: result.failed,
            frozen: result.frozen,
            mode: result.mode.as_str(),
            not_frozen: result.not_frozen,
            pre_frozen: result.pre_frozen,
            requested: result.requested,
            sweep_ms: result.sweep.as_millis() as i64,
            truncated: result.truncated,
            unobservable: result.unobservable,
            vanished: result.vanished,
            visited: result.visited,
            wait_ms: result.wait.as_millis() as i64,
        }
    }
}
