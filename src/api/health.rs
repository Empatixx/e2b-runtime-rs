use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

pub async fn get_health() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")])
}

/// Phase 2 endpoints (freeze, fsfreeze, collapse, metrics) are not implemented yet.
pub async fn not_implemented() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
