use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::app::AppState;
use crate::upgrade::DEFAULT_UPGRADE_BIN;
use crate::upgrade::outgoing::upgrade;
use crate::util::error_response::ErrorResponse;
use crate::util::fs::os_error;

const UPGRADE_BIN_HEADER: &str = "X-Envd-Upgrade-Bin";

pub async fn post_upgrade(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    if !state.initialized.load(Ordering::SeqCst) {
        return plain(StatusCode::CONFLICT, "envd not initialized");
    }
    let target = headers
        .get(UPGRADE_BIN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !target.is_empty() && target != DEFAULT_UPGRADE_BIN {
        return plain(StatusCode::BAD_REQUEST, "unsupported upgrade target");
    }
    let new_bin = match carries_body(&headers) {
        true => match receive_binary(body).await {
            Ok(()) => Some(DEFAULT_UPGRADE_BIN),
            Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &e),
        },
        false => None,
    };
    let failure = upgrade(&state, new_bin).await;
    plain(StatusCode::INTERNAL_SERVER_ERROR, &failure)
}

fn carries_body(headers: &HeaderMap) -> bool {
    match headers
        .get(header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok())
    {
        Some(length) => length != "0",
        None => headers.contains_key(header::TRANSFER_ENCODING),
    }
}

async fn receive_binary(body: Body) -> Result<(), String> {
    let path = std::path::Path::new(DEFAULT_UPGRADE_BIN);
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(path)
        .map_err(|e| os_error("open", path, &e))?;
    let mut chunks = body.into_data_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).map_err(|e| os_error("write", path, &e))?;
    }
    Ok(())
}

fn plain(status: StatusCode, message: &str) -> Response {
    ErrorResponse::builder()
        .status(status)
        .message(message)
        .plain_text()
        .build()
        .into_response()
}
