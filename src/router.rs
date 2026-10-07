use std::sync::{Arc, Mutex};

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
    let filesystem = FilesystemService {
        state: state.clone(),
        watchers: Mutex::default(),
    };
    let connect = Arc::new(filesystem).register(connectrpc::Router::new());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let public = Router::new()
        .route("/health", get(health::health))
        .route("/init", post(init::post_init));

    let protected = Router::new()
        .route("/envs", get(envs::get_envs))
        .route("/metrics", get(health::not_implemented))
        .route("/freeze", post(health::not_implemented))
        .route("/unfreeze", post(health::not_implemented))
        .route("/fsfreeze", post(health::not_implemented))
        .route("/fsthaw", post(health::not_implemented))
        .route("/collapse", post(health::not_implemented))
        .fallback_service(connect.into_axum_service())
        .layer(from_fn_with_state(state.clone(), require_token));

    public
        .merge(protected)
        .with_state(state)
        .layer(from_fn(authenticate_user))
        .layer(cors)
}
