use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::host::fsfreeze::{self, ROOTFS_MOUNTPOINT};
use crate::util::error_response::ErrorResponse;

pub async fn post_fsfreeze(State(state): State<Arc<AppState>>) -> Result<Response, ErrorResponse> {
    let _locked = state.fs_freeze_lock.lock().await;
    run(fsfreeze::freeze, "freeze rootfs").await
}

pub async fn post_fsthaw(State(state): State<Arc<AppState>>) -> Result<Response, ErrorResponse> {
    let _locked = state.fs_freeze_lock.lock().await;
    run(fsfreeze::thaw, "thaw rootfs").await
}

async fn run(operation: fn(&Path) -> Result<(), String>, failure: &'static str) -> Result<Response, ErrorResponse> {
    tokio::task::spawn_blocking(move || operation(Path::new(ROOTFS_MOUNTPOINT)))
        .await
        .map_err(|e| e.to_string())
        .and_then(|done| done)
        .map(|()| (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")]).into_response())
        .map_err(|e| {
            tracing::error!(error = %e, "{failure}");
            ErrorResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .message(e)
                .build()
        })
}
