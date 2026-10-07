pub mod auth;
pub mod init;
pub mod signing;

use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health))
        .route("/init", post(init::post_init))
        .route("/envs", get(envs))
        .route("/metrics", get(not_implemented))
        .route("/freeze", post(not_implemented))
        .route("/unfreeze", post(not_implemented))
        .route("/fsfreeze", post(not_implemented))
        .route("/fsthaw", post(not_implemented))
        .route("/collapse", post(not_implemented))
}

async fn health() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")])
}

async fn envs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Json(state.env_vars.all()))
}

async fn not_implemented() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
