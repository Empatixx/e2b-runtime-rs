use std::sync::Arc;

use tower_http::cors::{Any, CorsLayer};

use crate::state::AppState;

/// One router on one port, like upstream's single `http.ServeMux`:
/// HTTP routes first, everything else falls through to the Connect services.
pub fn build_router(state: Arc<AppState>) -> axum::Router {
    let connect = connectrpc::Router::new();
    crate::api::routes()
        .with_state(state)
        .fallback_service(connect.into_axum_service())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
                .expose_headers(Any),
        )
}
