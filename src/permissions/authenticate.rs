//! Connect user from `Authorization: Basic` (upstream `permissions/authenticate.go`
//! wired as `authn.NewMiddleware(permissions.AuthenticateUsername)` in `main.go`).

use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use connectrpc::ConnectError;

use super::user::{self, UserInfo};
use crate::execcontext::resolve_default_username;

/// The looked-up Basic-auth user, stored as a request extension (Go: `authn.SetInfo(ctx, *user.User)`).
#[derive(Clone, Debug)]
pub struct AuthUser(pub UserInfo);

/// Upstream `authn.Middleware.Wrap` + `AuthenticateUsername`, for EVERY request (HTTP routes too).
///
/// No Basic auth -> pass through without a user. Unknown user -> Connect error
/// (`unauthenticated`, written like connect-go's `ErrorWriter`) and the handler never runs.
pub async fn authenticate_username(mut req: Request, next: Next) -> Response {
    let header_value = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let Some(username) = basic_username(header_value) else {
        return next.run(req).await;
    };
    match user::lookup(&username) {
        Ok(u) => {
            req.extensions_mut().insert(AuthUser(u));
            next.run(req).await
        }
        Err(_) => ConnectError::unauthenticated(format!("invalid username: '{username}'"))
            .into_http_response(req.headers())
            .map(axum::body::Body::new)
            .into_response(),
    }
}

/// Username from `Authorization: Basic ...`, same rules as Go's `http.Request.BasicAuth()`.
/// `None` = no usable Basic auth (the default user is used then).
pub fn basic_username(value: Option<&str>) -> Option<String> {
    const PREFIX: &str = "Basic ";
    let value = value?;
    // Go: ascii.EqualFold on the prefix, so "basic " / "BASIC " work too
    if !value.get(..PREFIX.len())?.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let bytes = STANDARD.decode(&value[PREFIX.len()..]).ok()?;
    let string = String::from_utf8(bytes).ok()?;
    // Go: strings.Cut(cs, ":") — no colon means ok=false, i.e. no user at all
    let (username, _password) = string.split_once(':')?;
    Some(username.to_owned())
}

/// Upstream `GetAuthUser`: the user from the middleware, otherwise the default user from `/init`.
pub fn get_auth_user(auth: Option<&AuthUser>, default_user: &str) -> Result<UserInfo, ConnectError> {
    if let Some(AuthUser(u)) = auth {
        return Ok(u.clone());
    }
    let name = resolve_default_username(None, default_user)
        .map_err(|_| ConnectError::unauthenticated("no user specified"))?;
    user::lookup(&name).map_err(|_| ConnectError::unauthenticated(format!("invalid default user: '{name}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_username_like_go_parse_basic_auth() {
        // "user:pass"
        assert_eq!(basic_username(Some("Basic dXNlcjpwYXNz")), Some("user".into()));
        // "user:" — E2B SDK sends an empty password
        assert_eq!(basic_username(Some("Basic dXNlcjo=")), Some("user".into()));
        // ":" — empty username is still "present" in Go (then fails the lookup)
        assert_eq!(basic_username(Some("Basic Og==")), Some("".into()));
        // Go: the "Basic " prefix is case-insensitive (ascii.EqualFold)
        assert_eq!(basic_username(Some("basic dXNlcjo=")), Some("user".into()));
        assert_eq!(basic_username(Some("BASIC dXNlcjo=")), Some("user".into()));
        // "root" — no colon: Go's strings.Cut fails -> ok=false -> NO user
        assert_eq!(basic_username(Some("Basic cm9vdA==")), None);
        assert_eq!(basic_username(None), None);
        assert_eq!(basic_username(Some("Bearer abc")), None, "not Basic");
        assert_eq!(basic_username(Some("Basic")), None, "too short");
        assert_eq!(basic_username(Some("Basic !!!")), None, "invalid base64");
    }

    #[test]
    fn auth_user_from_middleware_wins() {
        let me = user::lookup("root").unwrap();
        assert_eq!(get_auth_user(Some(&AuthUser(me.clone())), "").unwrap(), me);
    }

    #[test]
    fn falls_back_to_default_user() {
        assert_eq!(get_auth_user(None, "root").unwrap().name, "root");
    }

    #[test]
    fn empty_default_user_is_unauthenticated() {
        let err = get_auth_user(None, "").unwrap_err();
        assert_eq!(err.code, connectrpc::ErrorCode::Unauthenticated);
        assert_eq!(err.message.as_deref(), Some("no user specified"));
    }

    #[test]
    fn unknown_default_user_is_unauthenticated() {
        let err = get_auth_user(None, "definitely-missing-user").unwrap_err();
        assert_eq!(err.code, connectrpc::ErrorCode::Unauthenticated);
        assert_eq!(err.message.as_deref(), Some("invalid default user: 'definitely-missing-user'"));
    }
}
