use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use crate::state::AppState;

pub async fn get_envs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Json(state.env_vars.all()))
}
