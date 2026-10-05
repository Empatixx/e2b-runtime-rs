use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use tower_http::cors::{Any, CorsLayer};

use crate::api::auth::require_token;
use crate::state::AppState;

/// One router on one port, like upstream's single `http.ServeMux`:
/// HTTP routes first, everything else falls through to the Connect services.
pub fn build_router(state: Arc<AppState>) -> axum::Router {
    let connect = connectrpc::Router::new();
    crate::api::routes()
        .with_state(state.clone())
        .fallback_service(connect.into_axum_service())
        // auth wraps routes + Connect; added before CORS so CORS is the outer layer
        // and browser preflight (OPTIONS) is answered without a token
        .layer(from_fn_with_state(state, require_token))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers(Any),
        )
}
