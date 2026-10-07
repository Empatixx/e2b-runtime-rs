use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

pub async fn health() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")])
}

pub async fn not_implemented() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
