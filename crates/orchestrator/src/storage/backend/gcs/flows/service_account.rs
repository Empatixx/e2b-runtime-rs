use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use serde::Deserialize;

use super::TokenFlow;
use super::response::read_two_legged;
use super::service_key::ServiceKey;

const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_UNIVERSE_DOMAIN: &str = "googleapis.com";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const SKEW_SECONDS: u64 = 10;
const LIFETIME_SECONDS: u64 = 3600;

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServiceAccountFile {
    private_key_id: String,
    private_key: String,
    client_email: String,
    token_uri: String,
    universe_domain: String,
}

enum Mode {
    TokenExchange { token_url: String, client: Client },
    SelfSigned { key: Box<ServiceKey> },
}

pub struct ServiceAccountFlow {
    file: ServiceAccountFile,
    scopes: Vec<String>,
    mode: Mode,
}

impl ServiceAccountFlow {
    pub fn from_json(json: &[u8], scopes: &[&str]) -> Result<Arc<Self>, String> {
        let file: ServiceAccountFile = serde_json::from_slice(json).map_err(|e| e.to_string())?;
        let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        let self_signed = !file.universe_domain.is_empty() && file.universe_domain != DEFAULT_UNIVERSE_DOMAIN;
        let mode = match self_signed {
            true => Self::self_signed(&file, &scopes)?,
            false => Self::token_exchange(&file)?,
        };
        Ok(Arc::new(Self { file, scopes, mode }))
    }

    fn self_signed(file: &ServiceAccountFile, scopes: &[String]) -> Result<Mode, String> {
        if scopes.is_empty() {
            return Err("credentials: both scopes and audience are empty".to_owned());
        }
        ServiceKey::parse(&file.private_key)
            .map(|key| Mode::SelfSigned { key: Box::new(key) })
            .map_err(|e| format!("credentials: could not parse key: {e}"))
    }

    fn token_exchange(file: &ServiceAccountFile) -> Result<Mode, String> {
        if file.client_email.is_empty() {
            return Err("auth: email must be provided".to_owned());
        }
        if file.private_key.is_empty() {
            return Err("auth: private key must be provided".to_owned());
        }
        let token_url = match file.token_uri.is_empty() {
            true => DEFAULT_TOKEN_URL.to_owned(),
            false => file.token_uri.clone(),
        };
        Ok(Mode::TokenExchange {
            token_url,
            client: Client::new(),
        })
    }

    fn jwt(&self, key: &ServiceKey, claims: &str) -> Result<String, String> {
        let header = format!(
            r#"{{"alg":"RS256","typ":"JWT","kid":{}}}"#,
            json(&self.file.private_key_id)
        );
        let signing_input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(claims));
        let signature = key.sign(signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    fn assertion(&self, token_url: &str, now_unix: u64) -> Result<String, String> {
        let issued = now_unix - SKEW_SECONDS;
        let claims = format!(
            r#"{{"iss":{},"scope":{},"exp":{},"iat":{issued},"aud":{}}}"#,
            json(&self.file.client_email),
            json(&self.scopes.join(" ")),
            issued + LIFETIME_SECONDS,
            json(token_url)
        );
        let key = ServiceKey::parse(&self.file.private_key)?;
        self.jwt(&key, &claims)
    }

    fn self_signed_token(&self, key: &ServiceKey, now_unix: u64) -> Result<(String, Option<SystemTime>), String> {
        let claims = format!(
            r#"{{"iss":{email},"scope":{},"exp":{},"iat":{now_unix},"aud":"","sub":{email}}}"#,
            json(&self.scopes.join(" ")),
            now_unix + LIFETIME_SECONDS,
            email = json(&self.file.client_email)
        );
        let expiry = UNIX_EPOCH + Duration::from_secs(now_unix + LIFETIME_SECONDS);
        self.jwt(key, &claims)
            .map(|token| (token, Some(expiry)))
            .map_err(|e| format!("credentials: could not encode JWT: {e}"))
    }
}

fn json(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[async_trait::async_trait]
impl TokenFlow for ServiceAccountFlow {
    async fn token(&self) -> Result<(String, Option<SystemTime>), String> {
        match &self.mode {
            Mode::SelfSigned { key } => self.self_signed_token(key, now_unix()),
            Mode::TokenExchange { token_url, client } => {
                let assertion = self.assertion(token_url, now_unix())?;
                let response = client
                    .post(token_url)
                    .form(&[("grant_type", GRANT_TYPE), ("assertion", assertion.as_str())])
                    .send()
                    .await
                    .map_err(|e| format!("auth: cannot fetch token: {e}"))?;
                read_two_legged(response)
                    .await
                    .map(|token| (token.access_token, token.expiry))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey};

    use crate::storage::backend::pem;

    use super::*;

    const KEY: &str = include_str!("../test_key.pem");

    fn flow(json: &str) -> Arc<ServiceAccountFlow> {
        ServiceAccountFlow::from_json(json.as_bytes(), &["scope-a", "scope-b"]).unwrap()
    }

    #[test]
    fn required_fields_are_validated_with_the_sdk_texts() {
        assert_eq!(
            ServiceAccountFlow::from_json(b"{}", &[]).err().unwrap(),
            "auth: email must be provided"
        );
        assert_eq!(
            ServiceAccountFlow::from_json(br#"{"client_email":"a@b"}"#, &[])
                .err()
                .unwrap(),
            "auth: private key must be provided"
        );
    }

    #[test]
    fn the_assertion_is_an_rs256_jwt_over_the_token_url() {
        let json = serde_json::json!({"client_email": "sa@p.iam", "private_key": KEY, "private_key_id": "kid1", "token_uri": "https://tokens/x"});
        let flow = flow(&json.to_string());

        let jwt = flow.assertion("https://tokens/x", 1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();

        let header = String::from_utf8(URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header, r#"{"alg":"RS256","typ":"JWT","kid":"kid1"}"#);
        let claims = String::from_utf8(URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            claims,
            r#"{"iss":"sa@p.iam","scope":"scope-a scope-b","exp":1700003590,"iat":1699999990,"aud":"https://tokens/x"}"#
        );
        let der = pem::blocks(KEY).remove(0).der;
        let public = RsaKeyPair::from_pkcs8(&der).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public.public().as_ref())
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn non_default_universe_domains_get_a_self_signed_jwt_without_the_network() {
        let json = serde_json::json!({"client_email": "sa@p.iam", "private_key": KEY, "private_key_id": "kid1", "universe_domain": "example.com"});
        let flow = flow(&json.to_string());

        let (token, expiry) = flow
            .self_signed_token(
                match &flow.mode {
                    Mode::SelfSigned { key } => key,
                    Mode::TokenExchange { .. } => panic!("expected the self-signed mode"),
                },
                1_700_000_000,
            )
            .unwrap();

        let claims = String::from_utf8(URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(
            claims,
            r#"{"iss":"sa@p.iam","scope":"scope-a scope-b","exp":1700003600,"iat":1700000000,"aud":"","sub":"sa@p.iam"}"#
        );
        assert_eq!(expiry, Some(UNIX_EPOCH + Duration::from_secs(1_700_003_600)));
    }

    #[test]
    fn self_signed_mode_validates_scopes_and_the_key_up_front() {
        let bad_key =
            serde_json::json!({"client_email": "a@b", "private_key": "nope", "universe_domain": "example.com"});

        assert_eq!(
            ServiceAccountFlow::from_json(bad_key.to_string().as_bytes(), &[])
                .err()
                .unwrap(),
            "credentials: both scopes and audience are empty"
        );
        assert!(
            ServiceAccountFlow::from_json(bad_key.to_string().as_bytes(), &["s"])
                .err()
                .unwrap()
                .starts_with(
                    "credentials: could not parse key: failed to parse private key. Tried PKCS8, PKCS1, and EC formats."
                )
        );
    }
}
