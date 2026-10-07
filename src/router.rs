use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use tower_http::cors::{Any, CorsLayer};

use crate::app::AppState;
use crate::controllers::filesystem::FilesystemService;
use crate::controllers::{envs, health, init};
use crate::middleware::access_token::require_token;
use crate::middleware::basic_auth::authenticate_user;
use crate::proto::filesystem::FilesystemExt;

pub fn build_router(state: Arc<AppState>) -> Router {
    let connect = Arc::new(FilesystemService { state: state.clone() }).register(connectrpc::Router::new());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    Router::new()
        .route("/health", get(health::health))
        .route("/init", post(init::post_init))
        .route("/envs", get(envs::get_envs))
        .route("/metrics", get(health::not_implemented))
        .route("/freeze", post(health::not_implemented))
        .route("/unfreeze", post(health::not_implemented))
        .route("/fsfreeze", post(health::not_implemented))
        .route("/fsthaw", post(health::not_implemented))
        .route("/collapse", post(health::not_implemented))
        .with_state(state.clone())
        .fallback_service(connect.into_axum_service())
        .layer(from_fn(authenticate_user))
        .layer(from_fn_with_state(state, require_token))
        .layer(cors)
}
