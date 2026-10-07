use std::sync::Arc;

use axum::middleware::{from_fn, from_fn_with_state};
use tower_http::cors::{Any, CorsLayer};

use crate::http::auth::{authenticate_user, require_token};
use crate::proto::filesystem::FilesystemExt;
use crate::rpc::filesystem::FilesystemService;
use crate::state::AppState;

pub fn build_router(state: Arc<AppState>) -> axum::Router {
    let connect = Arc::new(FilesystemService { state: state.clone() }).register(connectrpc::Router::new());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    crate::http::routes()
        .with_state(state.clone())
        .fallback_service(connect.into_axum_service())
        .layer(from_fn(authenticate_user))
        .layer(from_fn_with_state(state, require_token))
        .layer(cors)
}
