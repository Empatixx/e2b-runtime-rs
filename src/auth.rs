use axum::http::{HeaderMap, header};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use connectrpc::ConnectError;
use sha2::{Digest, Sha256};
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
    if let Some(user) = basic_user {
        return Ok(user.clone());
    }
    if default_user.is_empty() {
        return Err(ConnectError::unauthenticated("no user specified"));
    }
    User::lookup(default_user)
        .ok_or_else(|| ConnectError::unauthenticated(format!("invalid default user: '{default_user}'")))
}

pub fn generate_signature(path: &str, username: &str, operation: &str, token: &str, expiration: Option<i64>) -> String {
    let input = match expiration {
        Some(expiration) => format!("{path}:{operation}:{username}:{token}:{expiration}"),
        None => format!("{path}:{operation}:{username}:{token}"),
    };
    format!("v1_{}", STANDARD_NO_PAD.encode(Sha256::digest(input)))
}

#[allow(clippy::too_many_arguments)]
pub fn validate_signing(
    token: Option<&str>,
    header_token: Option<&str>,
    signature: Option<&str>,
    expiration: Option<i64>,
    username: &str,
    path: &str,
    operation: &str,
    now_unix: i64,
) -> Result<(), &'static str> {
    let Some(token) = token else {
        return Ok(());
    };

    if let Some(header_token) = header_token.filter(|t| !t.is_empty()) {
        if bool::from(header_token.as_bytes().ct_eq(token.as_bytes())) {
            return Ok(());
        }
        return Err("access token present in header but does not match");
    }

    let Some(signature) = signature else {
        return Err("missing signature query parameter");
    };
    let expected = generate_signature(path, username, operation, token, expiration);
    if !bool::from(signature.as_bytes().ct_eq(expected.as_bytes())) {
        return Err("invalid signature");
    }
    if expiration.is_some_and(|expiration| expiration < now_unix) {
        return Err("signature is already expired");
    }
    Ok(())
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
        let sig = generate_signature("/p", "user", READ, "t", Some(100));
        let validate = |token, header, signature, operation, now| {
            validate_signing(token, header, signature, Some(100), "user", "/p", operation, now)
        };

        assert_eq!(validate(None, None, None, READ, 0), Ok(()));
        assert_eq!(validate(Some("t"), Some("t"), None, READ, 0), Ok(()));
        assert_eq!(
            validate(Some("t"), Some("x"), None, READ, 0),
            Err("access token present in header but does not match")
        );
        assert_eq!(
            validate(Some("t"), None, None, READ, 0),
            Err("missing signature query parameter")
        );
        assert_eq!(validate(Some("t"), None, Some(&sig), READ, 50), Ok(()));
        assert_eq!(
            validate(Some("t"), None, Some(&sig), READ, 101),
            Err("signature is already expired")
        );
        assert_eq!(
            validate(Some("t"), None, Some(&sig), WRITE, 50),
            Err("invalid signature")
        );
    }
}
