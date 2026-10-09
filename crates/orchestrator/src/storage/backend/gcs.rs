use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;

use crate::builder::oci::ecr::base64_error;
use crate::go_url::GoUrl;
use crate::go_url::query::Query;

const SERVICE_ACCOUNT_ENV: &str = "GOOGLE_SERVICE_ACCOUNT_BASE64";
const EMULATOR_HOST_ENV: &str = "STORAGE_EMULATOR_HOST";
const DEFAULT_HOST: &str = "storage.googleapis.com";
const PEM_BEGIN: &str = "-----BEGIN ";
const PEM_END: &str = "-----END ";
const PEM_DASHES: &str = "-----";
const SERVICE_ACCOUNT_TYPE: &str = "storage.gcpServiceToken";
const KEY_SYNTAX_ERROR: &str = "asn1: syntax error: sequence truncated";
const MISSING_ACCESS_ID: &str = "storage: missing required GoogleAccessID";

#[derive(Debug, Clone)]
pub struct ServiceAccountSigner {
    pub bucket: String,
    pub service_account_base64: String,
}

#[derive(Deserialize)]
struct ServiceAccount {
    #[serde(default)]
    client_email: String,
    #[serde(default)]
    private_key: String,
}

impl ServiceAccountSigner {
    pub fn from_env(bucket: &str) -> Self {
        Self {
            bucket: bucket.to_owned(),
            service_account_base64: std::env::var(SERVICE_ACCOUNT_ENV).unwrap_or_default(),
        }
    }

    pub fn upload_url(&self, path: &str, expires_in: Duration) -> Result<String, String> {
        let account = self
            .service_account()
            .map_err(|e| format!("failed to parse GCP service account: {e}"))?;
        let expires = SystemTime::now()
            .checked_add(expires_in)
            .and_then(|expiry| expiry.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since_epoch| since_epoch.as_secs());
        self.signed_url(&account, path, expires)
            .map_err(|e| format!("failed to create signed URL for GCS object ({path}): {e}"))
    }

    fn service_account(&self) -> Result<ServiceAccount, String> {
        let decoded = STANDARD.decode(&self.service_account_base64).map_err(|e| {
            format!(
                "failed to decode base64: {}",
                base64_error(&self.service_account_base64, e)
            )
        })?;
        serde_json::from_slice(&decoded)
            .map_err(|e| format!("failed to parse service account JSON: {}", json_error(&e)))
    }

    fn signed_url(&self, account: &ServiceAccount, path: &str, expires: u64) -> Result<String, String> {
        if account.client_email.is_empty() {
            return Err(MISSING_ACCESS_ID.to_owned());
        }
        let resource = GoUrl {
            path: format!("/{}/{path}", self.bucket),
            ..GoUrl::default()
        }
        .to_string();
        let signature = sign(&account.private_key, &string_to_sign(expires, &resource))?;
        let mut query = Query::single("GoogleAccessId", &account.client_email);
        query.values.insert("Expires".to_owned(), vec![expires.to_string()]);
        query.values.insert("Signature".to_owned(), vec![signature]);
        Ok(GoUrl {
            scheme: "https".to_owned(),
            host: host(),
            path: format!("/{}/{path}", self.bucket),
            raw_query: query.encode(),
            ..GoUrl::default()
        }
        .to_string())
    }
}

fn string_to_sign(expires: u64, resource: &str) -> String {
    format!("PUT\n\n\n{expires}\n{resource}")
}

fn host() -> String {
    let emulator = std::env::var(EMULATOR_HOST_ENV).ok().filter(|host| !host.is_empty());
    match emulator {
        Some(host) => host.split_once("://").map_or(host.clone(), |(_, rest)| rest.to_owned()),
        None => DEFAULT_HOST.to_owned(),
    }
}

fn sign(private_key: &str, message: &str) -> Result<String, String> {
    let der = private_key_der(private_key);
    let key = RsaKeyPair::from_pkcs8(&der)
        .or_else(|_| RsaKeyPair::from_der(&der))
        .map_err(|_| KEY_SYNTAX_ERROR.to_owned())?;
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|e| e.to_string())?;
    Ok(STANDARD.encode(signature))
}

fn private_key_der(private_key: &str) -> Vec<u8> {
    pem_body(private_key)
        .and_then(|body| STANDARD.decode(body).ok())
        .unwrap_or_else(|| private_key.as_bytes().to_vec())
}

fn pem_body(text: &str) -> Option<String> {
    let begin = text.find(PEM_BEGIN)?;
    let after_label = text[begin + PEM_BEGIN.len()..].split_once(PEM_DASHES)?.1;
    let body = after_label.split_once(PEM_END)?.0;
    Some(body.split_whitespace().collect())
}

fn json_error(error: &serde_json::Error) -> String {
    use serde_json::error::Category;
    match error.classify() {
        Category::Eof => "unexpected end of JSON input".to_owned(),
        Category::Data => format!(
            "json: cannot unmarshal {} into Go value of type {SERVICE_ACCOUNT_TYPE}",
            data_kind(error)
        ),
        Category::Syntax | Category::Io => {
            format!("invalid character {} looking for beginning of value", offending(error))
        }
    }
}

fn data_kind(error: &serde_json::Error) -> &'static str {
    let text = error.to_string();
    match text.split_once("invalid type: ").map(|(_, rest)| rest) {
        Some(rest) if rest.starts_with("string") => "string",
        Some(rest) if rest.starts_with("integer") || rest.starts_with("floating") => "number",
        Some(rest) if rest.starts_with("boolean") => "bool",
        Some(rest) if rest.starts_with("sequence") => "array",
        _ => "object",
    }
}

fn offending(error: &serde_json::Error) -> String {
    format!("'{}'", error.to_string().rsplit('`').nth(1).unwrap_or("?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer(secret: &str) -> ServiceAccountSigner {
        ServiceAccountSigner {
            bucket: "bucket".to_owned(),
            service_account_base64: secret.to_owned(),
        }
    }

    fn encoded(json: &str) -> String {
        STANDARD.encode(json)
    }

    #[test]
    fn a_missing_secret_reads_like_go_json_on_empty_input() {
        assert_eq!(
            signer("").upload_url("t/memfile", Duration::from_secs(60)).unwrap_err(),
            "failed to parse GCP service account: failed to parse service account JSON: unexpected end of JSON input"
        );
    }

    #[test]
    fn bad_base64_names_the_offending_byte() {
        assert_eq!(
            signer("a*b=")
                .upload_url("t/memfile", Duration::from_secs(60))
                .unwrap_err(),
            "failed to parse GCP service account: failed to decode base64: illegal base64 data at input byte 1"
        );
    }

    #[test]
    fn an_account_without_an_email_is_rejected_by_the_signer() {
        let secret = encoded(r#"{"private_key":"x"}"#);

        assert_eq!(
            signer(&secret)
                .upload_url("t/memfile", Duration::from_secs(60))
                .unwrap_err(),
            "failed to create signed URL for GCS object (t/memfile): storage: missing required GoogleAccessID"
        );
    }

    #[test]
    fn an_unreadable_key_is_a_signing_error() {
        let secret = encoded(r#"{"client_email":"sa@p.iam.gserviceaccount.com","private_key":"nope"}"#);

        assert_eq!(
            signer(&secret)
                .upload_url("t/memfile", Duration::from_secs(60))
                .unwrap_err(),
            "failed to create signed URL for GCS object (t/memfile): asn1: syntax error: sequence truncated"
        );
    }

    #[test]
    fn urls_are_v2_path_style_with_sorted_query() {
        let account = ServiceAccount {
            client_email: "sa@p.iam.gserviceaccount.com".to_owned(),
            private_key: TEST_KEY.to_owned(),
        };

        let url = signer("").signed_url(&account, "t/mem file", 1_700_000_000).unwrap();

        assert!(url.starts_with("https://storage.googleapis.com/bucket/t/mem%20file?Expires=1700000000&GoogleAccessId=sa%40p.iam.gserviceaccount.com&Signature="));
    }

    #[test]
    fn the_string_to_sign_is_the_v2_put_form() {
        assert_eq!(
            string_to_sign(1_700_000_000, "/bucket/t/mem%20file"),
            "PUT\n\n\n1700000000\n/bucket/t/mem%20file"
        );
    }

    #[test]
    fn signatures_verify_against_the_public_key() {
        use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
        let der = private_key_der(TEST_KEY);
        let key = RsaKeyPair::from_pkcs8(&der).unwrap();
        let message = string_to_sign(1_700_000_000, "/bucket/t/memfile");

        let signature = STANDARD.decode(sign(TEST_KEY, &message).unwrap()).unwrap();

        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref())
            .verify(message.as_bytes(), &signature)
            .unwrap();
    }

    const TEST_KEY: &str = include_str!("gcs/test_key.pem");
}
