//! Access-token check for every request (upstream `internal/api/auth.go` `WithAuthorization`)
//! and extraction of the Connect user from `Authorization: Basic` (upstream `permissions/authenticate.go`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use subtle::ConstantTimeEq;

use crate::state::AppState;

const ACCESS_TOKEN_HEADER: &str = "x-access-token";

/// Paths reachable without the token (they have their own checks: signing, MMDS).
const EXCLUDED: &[(&str, &str)] = &[("GET", "/health"), ("GET", "/files"), ("POST", "/files"), ("POST", "/init")];

/// Username from `Authorization: Basic`, stored as a request extension for later handlers.
#[derive(Clone, Debug)]
pub struct BasicUser(pub Option<String>);

/// Same JSON shape as upstream `jsonError`: `{"code": 401, "message": "..."}`.
pub fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "code": status.as_u16(), "message": message }))).into_response()
}

pub async fn require_token(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let header_value = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let user = basic_username(header_value);
    req.extensions_mut().insert(BasicUser(user));

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

/// `"Basic dXNlcjpwYXNz"` -> base64 decode -> `"user:pass"` -> `Some("user")`.
/// Anything malformed -> `None` (the default user is used then).
pub fn basic_username(value: Option<&str>) -> Option<String> {
    let bytes = STANDARD.decode(value?.strip_prefix("Basic ")?)
        .ok()?;
    let string = String::from_utf8(bytes)
        .ok()?;
    let splits = string.split_once(":");
    match splits {
        Some((username, _)) => Some(username.to_owned()),
        None => Some(string.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_username_cases() {
        // "user:pass"
        assert_eq!(basic_username(Some("Basic dXNlcjpwYXNz")), Some("user".into()));
        // "user:" — E2B SDK sends an empty password
        assert_eq!(basic_username(Some("Basic dXNlcjo=")), Some("user".into()));
        // "root" — no colon at all
        assert_eq!(basic_username(Some("Basic cm9vdA==")), Some("root".into()));
        assert_eq!(basic_username(None), None);
        assert_eq!(basic_username(Some("Bearer abc")), None, "not Basic");
        assert_eq!(basic_username(Some("Basic !!!")), None, "invalid base64");
    }
}
