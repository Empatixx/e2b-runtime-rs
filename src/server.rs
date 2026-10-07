use std::sync::Arc;

use axum::middleware::{from_fn, from_fn_with_state};
use tower_http::cors::{Any, CorsLayer};

use crate::api::auth::require_token;
use crate::permissions::authenticate::authenticate_username;
use crate::proto::filesystem::FilesystemExt;
use crate::services::filesystem::FilesystemService;
use crate::state::AppState;

/// One router on one port, like upstream's single `http.ServeMux`:
/// HTTP routes first, everything else falls through to the Connect services.
pub fn build_router(state: Arc<AppState>) -> axum::Router {
    let connect = Arc::new(FilesystemService::new(state.clone())).register(connectrpc::Router::new());
    crate::api::routes()
        .with_state(state.clone())
        .fallback_service(connect.into_axum_service())
        // same order as upstream main.go: withCORS(WithAuthorization(authn(handler))).
        // Layers added first are inner: Basic user lookup runs after the token check,
        // CORS is outermost so browser preflight (OPTIONS) is answered without a token.
        .layer(from_fn(authenticate_username))
        .layer(from_fn_with_state(state, require_token))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers(Any),
        )
}
