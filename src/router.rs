use std::sync::Arc;

use parking_lot::Mutex;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use tower_http::cors::{Any, CorsLayer};

use crate::app::AppState;
use crate::controllers::filesystem::FilesystemService;
use crate::controllers::process::ProcessService;
use crate::controllers::{envs, files, health, init};
use crate::middleware::access_token::require_token;
use crate::middleware::basic_auth::authenticate_user;
use crate::proto::filesystem::FilesystemExt;
use crate::proto::process::ProcessExt;

pub fn build_router(state: Arc<AppState>) -> Router {
    let filesystem = FilesystemService {
        state: state.clone(),
        watchers: Mutex::default(),
    };
    let process = ProcessService::new(state.clone());
    let connect = Arc::new(process).register(Arc::new(filesystem).register(connectrpc::Router::new()));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let public = Router::new()
        .route("/health", get(health::health))
        .route("/init", post(init::post_init))
        .route(
            "/files",
            get(files::download::get_files).post(files::upload::post_files),
        );

    let protected = Router::new()
        .route("/envs", get(envs::get_envs))
        .route("/files/compose", post(files::compose::post_files_compose))
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
