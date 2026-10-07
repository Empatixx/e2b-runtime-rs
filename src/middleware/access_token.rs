use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::app::AppState;

const ACCESS_TOKEN_HEADER: &str = "x-access-token";

pub async fn require_token(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let Some(token) = state.access_token() else {
        return next.run(request).await;
    };

    let provided = request
        .headers()
        .get(ACCESS_TOKEN_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();

    if bool::from(provided.ct_eq(token.as_bytes())) {
        return next.run(request).await;
    }

    tracing::error!("Trying to access secured envd without correct access token");
    let message = "unauthorized access, please provide a valid access token or method signing if supported";
    let status = StatusCode::UNAUTHORIZED;
    (
        status,
        Json(serde_json::json!({ "code": status.as_u16(), "message": message })),
    )
        .into_response()
}
