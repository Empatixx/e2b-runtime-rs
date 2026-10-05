//! Signed URLs for `/files` (upstream `internal/api/auth.go`):
//! `"v1_" + base64(no padding, sha256("path:operation:username:token[:expiration]"))`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const READ: &str = "read";
pub const WRITE: &str = "write";

pub fn generate_signature(path: &str, username: &str, operation: &str, token: &str, expiration: Option<i64>) -> String {
    let exp = match expiration {
        None => String::new(),
        Some(i) => format!(":{i}"),
    };
    let hash_input = format!("{}:{}:{}:{}{}", path, operation, username, token, exp);
    let hash = Sha256::digest(hash_input.as_bytes());
    let encoded = STANDARD_NO_PAD.encode(hash);
    let result = format!("v1_{}", encoded);
    result
}

/// `token_set` = token from `/init` (None = auth disabled); `header_token` = `X-Access-Token`.
#[allow(clippy::too_many_arguments)]
pub fn validate_signing(
    token_set: Option<&str>,
    header_token: Option<&str>,
    signature: Option<&str>,
    expiration: Option<i64>,
    username: Option<&str>,
    path: &str,
    operation: &str,
    now_unix: i64,
) -> Result<(), String> {
    let Some(token) = token_set else {
        return Ok(());
    };
    if let Some(header) = header_token.filter(|h| !h.is_empty()) {
        return if bool::from(header.as_bytes().ct_eq(token.as_bytes())) {
            Ok(())
        } else {
            Err("access token present in header but does not match".into())
        };
    }
    let signature = signature.ok_or("missing signature query parameter")?;
    let expected = generate_signature(path, username.unwrap_or(""), operation, token, expiration);
    if !bool::from(expected.as_bytes().ct_eq(signature.as_bytes())) {
        return Err("invalid signature".into());
    }
    if expiration.is_some_and(|exp| exp < now_unix) {
        return Err("signature is already expired".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_go_vectors() {
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
    fn validate_rules() {
        let sig = generate_signature("/p", "user", READ, "t", Some(100));
        assert!(validate_signing(None, None, None, None, None, "/p", READ, 0).is_ok(), "no token set → open");
        assert!(validate_signing(Some("t"), Some("t"), None, None, None, "/p", READ, 0).is_ok());
        assert_eq!(
            validate_signing(Some("t"), Some("x"), None, None, None, "/p", READ, 0).unwrap_err(),
            "access token present in header but does not match"
        );
        assert_eq!(
            validate_signing(Some("t"), None, None, None, None, "/p", READ, 0).unwrap_err(),
            "missing signature query parameter"
        );
        assert!(validate_signing(Some("t"), None, Some(&sig), Some(100), Some("user"), "/p", READ, 50).is_ok());
        assert_eq!(
            validate_signing(Some("t"), None, Some(&sig), Some(100), Some("user"), "/p", READ, 101).unwrap_err(),
            "signature is already expired"
        );
        assert_eq!(
            validate_signing(Some("t"), None, Some(&sig), Some(100), Some("user"), "/p", WRITE, 50).unwrap_err(),
            "invalid signature"
        );
    }
}
