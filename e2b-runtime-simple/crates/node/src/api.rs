pub mod error;
pub mod forward;
pub mod models;
pub mod sandboxes;
pub mod volumes;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use axum::routing::{get, post};

use crate::host::Host;
use crate::sandbox::Sandboxes;

#[derive(Clone)]
pub struct ApiState {
    pub host: Arc<Host>,
    pub sandboxes: Arc<Sandboxes>,
    pub api_key: Arc<str>,
    pub http: reqwest::Client,
}

pub fn router(state: ApiState) -> Router {
    let one_sandbox = Router::new()
        .route("/sandboxes/{id}", get(sandboxes::detail).delete(sandboxes::kill))
        .route("/sandboxes/{id}/pause", post(sandboxes::pause))
        .route("/sandboxes/{id}/resume", post(sandboxes::resume))
        .route("/sandboxes/{id}/connect", post(sandboxes::connect))
        .route("/v2/sandboxes/{id}/connect", post(sandboxes::connect))
        .route("/sandboxes/{id}/timeout", post(sandboxes::set_timeout))
        .route("/sandboxes/{id}/refreshes", post(sandboxes::refresh))
        .route("/sandboxes/{id}/snapshots", post(sandboxes::snapshot))
        .route_layer(from_fn_with_state(state.clone(), forward::to_owner));
    let protected = Router::new()
        .route("/sandboxes", post(sandboxes::create).get(sandboxes::list_running))
        .route("/v2/sandboxes", post(sandboxes::create).get(sandboxes::list))
        .route("/volumes", post(volumes::create).get(volumes::list))
        .route("/volumes/{id}", get(volumes::detail).delete(volumes::delete))
        .merge(one_sandbox)
        .route_layer(from_fn_with_state(state.clone(), require_api_key));
    Router::new()
        .route("/health", get(|| async { StatusCode::OK }))
        .merge(protected)
        .with_state(state)
}

async fn require_api_key(State(state): State<ApiState>, request: Request, next: Next) -> Result<Response, StatusCode> {
    let presented = request.headers().get("X-API-Key").and_then(|key| key.to_str().ok());
    match presented == Some(&*state.api_key) {
        true => Ok(next.run(request).await),
        false => Err(StatusCode::UNAUTHORIZED),
    }
}
