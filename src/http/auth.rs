use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use connectrpc::ConnectError;
use subtle::ConstantTimeEq;

use crate::state::AppState;
use crate::user::User;

const ACCESS_TOKEN_HEADER: &str = "x-access-token";

const PUBLIC_ROUTES: &[(&str, &str)] = &[("GET", "/health"), ("GET", "/files"), ("POST", "/files"), ("POST", "/init")];

pub fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "code": status.as_u16(), "message": message }))).into_response()
}

pub async fn require_token(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let Some(token) = state.access_token() else {
        return next.run(request).await;
    };

    let is_public = PUBLIC_ROUTES
        .iter()
        .any(|&(method, path)| request.method() == method && request.uri().path() == path);
    let provided = request
        .headers()
        .get(ACCESS_TOKEN_HEADER)
        .map(|value| value.as_bytes())
        .unwrap_or_default();

    if is_public || bool::from(provided.ct_eq(token.as_bytes())) {
        return next.run(request).await;
    }

    tracing::error!("Trying to access secured envd without correct access token");
    json_error(
        StatusCode::UNAUTHORIZED,
        "unauthorized access, please provide a valid access token or method signing if supported",
    )
}

pub async fn authenticate_user(mut request: Request, next: Next) -> Response {
    let Some(name) = basic_username(request.headers()) else {
        return next.run(request).await;
    };

    match User::lookup(&name) {
        Some(user) => {
            request.extensions_mut().insert(user);
            next.run(request).await
        }
        None => ConnectError::unauthenticated(format!("invalid username: '{name}'"))
            .into_http_response(request.headers())
            .map(Body::new),
    }
}

pub fn basic_username(headers: &HeaderMap) -> Option<String> {
    const PREFIX: &str = "Basic ";

    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (prefix, encoded) = value.split_at_checked(PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(PREFIX) {
        return None;
    }

    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (username, _password) = decoded.split_once(':')?;
    Some(username.to_owned())
}

pub fn request_user(basic_user: Option<&User>, default_user: &str) -> Result<User, ConnectError> {
    if let Some(user) = basic_user {
        return Ok(user.clone());
    }
    if default_user.is_empty() {
        return Err(ConnectError::unauthenticated("no user specified"));
    }
    User::lookup(default_user)
        .ok_or_else(|| ConnectError::unauthenticated(format!("invalid default user: '{default_user}'")))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use connectrpc::ErrorCode;

    use super::*;

    fn username(authorization: &str) -> Option<String> {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(authorization).unwrap());
        basic_username(&headers)
    }

    #[test]
    fn basic_username_cases() {
        assert_eq!(username("Basic dXNlcjpwYXNz").as_deref(), Some("user"));
        assert_eq!(username("Basic dXNlcjo=").as_deref(), Some("user"));
        assert_eq!(username("Basic Og==").as_deref(), Some(""));
        assert_eq!(username("basic dXNlcjo=").as_deref(), Some("user"));
        assert_eq!(username("BASIC dXNlcjo=").as_deref(), Some("user"));
        assert_eq!(username("Basic cm9vdA=="), None);
        assert_eq!(username("Bearer abc"), None);
        assert_eq!(username("Basic"), None);
        assert_eq!(username("Basic !!!"), None);
        assert_eq!(basic_username(&HeaderMap::new()), None);
    }

    #[test]
    fn request_user_cases() {
        let root = User::lookup("root").unwrap();
        assert_eq!(request_user(Some(&root), "").unwrap(), root);
        assert_eq!(request_user(None, "root").unwrap(), root);

        let err = request_user(None, "").unwrap_err();
        assert_eq!(err.code, ErrorCode::Unauthenticated);
        assert_eq!(err.message.as_deref(), Some("no user specified"));

        let err = request_user(None, "definitely-missing-user").unwrap_err();
        assert_eq!(err.code, ErrorCode::Unauthenticated);
        assert_eq!(err.message.as_deref(), Some("invalid default user: 'definitely-missing-user'"));
    }
}
