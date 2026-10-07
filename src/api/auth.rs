//! Access-token check for every request (upstream `internal/api/auth.go` `WithAuthorization`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::state::AppState;

const ACCESS_TOKEN_HEADER: &str = "x-access-token";

/// Paths reachable without the token (they have their own checks: signing, MMDS).
const EXCLUDED: &[(&str, &str)] = &[("GET", "/health"), ("GET", "/files"), ("POST", "/files"), ("POST", "/init")];

/// Same JSON shape as upstream `jsonError`: `{"code": 401, "message": "..."}`.
pub fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "code": status.as_u16(), "message": message }))).into_response()
}

pub async fn require_token(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Some(token) = state.access_token() else {
        return next.run(req).await;
    };
    let excluded = EXCLUDED
        .iter()
        .any(|(method, path)| req.method().as_str() == *method && req.uri().path() == *path);
    let provided = req.headers().get(ACCESS_TOKEN_HEADER).map(|v| v.as_bytes()).unwrap_or_default();

    if !excluded && !bool::from(provided.ct_eq(token.as_bytes())) {
        tracing::error!("Trying to access secured envd without correct access token");
        return json_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized access, please provide a valid access token or method signing if supported",
        );
    }
    next.run(req).await
}
