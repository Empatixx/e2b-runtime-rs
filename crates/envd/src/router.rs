use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use connectrpc::{CompressionPolicy, CompressionRegistry, ConnectRpcService, GzipProvider};
use tower::Layer;

use crate::app::AppState;
use crate::controllers::filesystem::FilesystemService;
use crate::controllers::process::ProcessService;
use crate::controllers::{collapse, envs, files, freeze, fsfreeze, health, init, upgrade};
use crate::middleware::accept_encoding::advertise_encodings;
use crate::middleware::access_token::require_token;
use crate::middleware::basic_auth::authenticate_user;
use crate::middleware::connect_timeout::ConnectTimeout;
use crate::middleware::cors::cors;
use crate::middleware::legacy_sdk::legacy_sdk;
use crate::middleware::unary_framing::frame_unary;
use crate::proto::filesystem::FilesystemExt;
use crate::proto::process::ProcessExt;

pub fn build_router(state: Arc<AppState>) -> Router {
    let filesystem = FilesystemService { state: state.clone() };
    let process = ProcessService::new(state.clone());
    let connect =
        ConnectRpcService::new(Arc::new(process).register(Arc::new(filesystem).register(connectrpc::Router::new())))
            .with_compression(CompressionRegistry::new().register(GzipProvider::default()))
            .with_compression_policy(CompressionPolicy::default().with_min_size(0))
            .with_interceptor(ConnectTimeout);

    let public = Router::new()
        .route("/health", get(health::health))
        .route("/init", post(init::post_init))
        .route(
            "/files",
            get(files::download::get_files).post(files::upload::post_files),
        )
        .layer(from_fn(authenticate_user));

    let protected = Router::new()
        .route("/envs", get(envs::get_envs))
        .route("/files/compose", post(files::compose::post_files_compose))
        .route("/metrics", get(health::get_metrics))
        .route("/freeze", post(freeze::post_freeze))
        .route("/unfreeze", post(freeze::post_unfreeze))
        .route("/fsfreeze", post(fsfreeze::post_fsfreeze))
        .route("/fsthaw", post(fsfreeze::post_fsthaw))
        .route("/collapse", post(collapse::post_collapse))
        .route("/upgrade", post(upgrade::post_upgrade))
        .fallback_service(
            from_fn(legacy_sdk).layer(from_fn(advertise_encodings).layer(from_fn(frame_unary).layer(connect))),
        )
        .layer(from_fn(authenticate_user))
        .layer(from_fn_with_state(state.clone(), require_token));

    public
        .merge(protected)
        .with_state(state)
        .layer(DefaultBodyLimit::disable())
        .layer(from_fn(cors))
}
