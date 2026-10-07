use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::app::AppState;
use crate::util::error_response::ErrorResponse;

const ACCESS_TOKEN_HEADER: &str = "x-access-token";

pub async fn require_token(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let provided = request
        .headers()
        .get(ACCESS_TOKEN_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    let authorized = state
        .access_token()
        .is_none_or(|token| bool::from(provided.ct_eq(token.as_bytes())));

    if authorized {
        return next.run(request).await;
    }

    tracing::error!("Trying to access secured envd without correct access token");
    ErrorResponse::builder()
        .status(StatusCode::UNAUTHORIZED)
        .message("unauthorized access, please provide a valid access token or method signing if supported")
        .build()
        .into_response()
}
