use axum::http::{HeaderMap, header};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use connectrpc::ConnectError;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

use subtle::ConstantTimeEq;

use crate::user::User;

pub const READ: &str = "read";
pub const WRITE: &str = "write";

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
    match basic_user {
        Some(user) => Ok(user.clone()),
        None if default_user.is_empty() => Err(ConnectError::unauthenticated("no user specified")),
        None => User::lookup(default_user)
            .ok_or_else(|| ConnectError::unauthenticated(format!("invalid default user: '{default_user}'"))),
    }
}

pub fn generate_signature(path: &str, username: &str, operation: &str, token: &str, expiration: Option<i64>) -> String {
    let input = match expiration {
        Some(expiration) => format!("{path}:{operation}:{username}:{token}:{expiration}"),
        None => format!("{path}:{operation}:{username}:{token}"),
    };
    format!("v1_{}", STANDARD_NO_PAD.encode(Sha256::digest(input)))
}

pub fn validate_signing(
    token: Option<&str>,
    header_token: Option<&str>,
    signature: Option<&str>,
    expiration: Option<i64>,
    username: &str,
    path: &str,
    operation: &str,
) -> Result<(), &'static str> {
    let Some(token) = token else {
        return Ok(());
    };
    let matches = |given: &str, expected: &str| bool::from(given.as_bytes().ct_eq(expected.as_bytes()));

    if let Some(header_token) = header_token.filter(|t| !t.is_empty()) {
        return matches(header_token, token)
            .then_some(())
            .ok_or("access token present in header but does not match");
    }

    let signature = signature.ok_or("missing signature query parameter")?;
    let expected = generate_signature(path, username, operation, token, expiration);
    matches(signature, &expected).then_some(()).ok_or("invalid signature")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    expiration
        .is_none_or(|expiration| expiration >= now)
        .then_some(())
        .ok_or("signature is already expired")
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
        assert_eq!(
            err.message.as_deref(),
            Some("invalid default user: 'definitely-missing-user'")
        );
    }

    #[test]
    fn signatures_match_known_values() {
        assert_eq!(
            generate_signature("/home/user/a.txt", "user", READ, "secret-token", None),
            "v1_powbkN3fI7yRlWaB5Ui2dRUwVP8UZprfSGludREq8sk"
        );
        assert_eq!(
            generate_signature("/home/user/a.txt", "user", WRITE, "secret-token", Some(1790000000)),
            "v1_JuR4i0z5anWKCNUAkaqXV5j2MD5H1U97U5/JsV83PAs"
        );
    }

    #[test]
    fn signing_rules() {
        let future = Some(i64::MAX);
        let past = Some(100);
        let sig = generate_signature("/p", "user", READ, "t", future);
        let expired_sig = generate_signature("/p", "user", READ, "t", past);
        let validate = |token, header, signature, expiration, operation| {
            validate_signing(token, header, signature, expiration, "user", "/p", operation)
        };

        assert_eq!(validate(None, None, None, future, READ), Ok(()));
        assert_eq!(validate(Some("t"), Some("t"), None, future, READ), Ok(()));
        assert_eq!(
            validate(Some("t"), Some("x"), None, future, READ),
            Err("access token present in header but does not match")
        );
        assert_eq!(
            validate(Some("t"), None, None, future, READ),
            Err("missing signature query parameter")
        );
        assert_eq!(validate(Some("t"), None, Some(&sig), future, READ), Ok(()));
        assert_eq!(
            validate(Some("t"), None, Some(&expired_sig), past, READ),
            Err("signature is already expired")
        );
        assert_eq!(
            validate(Some("t"), None, Some(&sig), future, WRITE),
            Err("invalid signature")
        );
    }
}
