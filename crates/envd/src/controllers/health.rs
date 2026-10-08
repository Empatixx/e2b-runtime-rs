use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::host::metrics::{Metrics, read_metrics};
use crate::util::go_json;

pub async fn health() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")])
}

pub async fn get_metrics(State(state): State<Arc<AppState>>) -> Response {
    let headers = [
        (header::CACHE_CONTROL, "no-store"),
        (header::CONTENT_TYPE, "application/json"),
    ];
    let metrics = read_metrics(&state.cpu).map(|metrics| Metrics {
        oom_kills: state.oom_kills.as_ref().and_then(|watcher| watcher.kills()),
        ..metrics
    });
    match metrics
        .map_err(|e| e.to_string())
        .and_then(|metrics| go_json::encode(&metrics).map_err(|e| e.to_string()))
    {
        Ok(body) => (StatusCode::OK, headers, body).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get metrics");
            (StatusCode::INTERNAL_SERVER_ERROR, headers).into_response()
        }
    }
}

pub async fn not_implemented() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
