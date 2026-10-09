use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;

use super::TokenFlow;
use super::response::read_two_legged;
use crate::storage::backend::pem;

const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const KEY_ERROR: &str = "failed to parse private key. Tried PKCS8, PKCS1, and EC formats. Errors: [PKCS8: asn1: syntax error: data truncated], [PKCS1: asn1: syntax error: data truncated], [EC: asn1: syntax error: data truncated]";
const SKEW_SECONDS: u64 = 10;
const LIFETIME_SECONDS: u64 = 3600;

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServiceAccountFile {
    private_key_id: String,
    private_key: String,
    client_email: String,
    token_uri: String,
}

pub struct ServiceAccountFlow {
    file: ServiceAccountFile,
    token_url: String,
    scopes: Vec<String>,
    client: Client,
}

impl ServiceAccountFlow {
    pub fn from_json(json: &[u8], scopes: &[&str]) -> Result<Arc<Self>, String> {
        let file: ServiceAccountFile = serde_json::from_slice(json).map_err(|e| e.to_string())?;
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
        Ok(Arc::new(Self {
            file,
            token_url,
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            client: Client::new(),
        }))
    }

    fn assertion(&self, now_unix: u64) -> Result<String, String> {
        let issued = now_unix - SKEW_SECONDS;
        let header = format!(
            r#"{{"alg":"RS256","typ":"JWT","kid":{}}}"#,
            json(&self.file.private_key_id)
        );
        let claims = format!(
            r#"{{"iss":{},"scope":{},"exp":{},"iat":{issued},"aud":{}}}"#,
            json(&self.file.client_email),
            json(&self.scopes.join(" ")),
            issued + LIFETIME_SECONDS,
            json(&self.token_url)
        );
        let signing_input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(claims));
        let der = pem::blocks(&self.file.private_key)
            .into_iter()
            .next()
            .map_or_else(|| self.file.private_key.as_bytes().to_vec(), |block| block.der);
        let key = RsaKeyPair::from_pkcs8(&der)
            .or_else(|_| RsaKeyPair::from_der(&der))
            .map_err(|_| KEY_ERROR.to_owned())?;
        let mut signature = vec![0; key.public().modulus_len()];
        key.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|e| e.to_string())?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

fn json(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

#[async_trait::async_trait]
impl TokenFlow for ServiceAccountFlow {
    async fn token(&self) -> Result<(String, Option<SystemTime>), String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let assertion = self.assertion(now)?;
        let response = self
            .client
            .post(&self.token_url)
            .form(&[("grant_type", GRANT_TYPE), ("assertion", assertion.as_str())])
            .send()
            .await
            .map_err(|e| format!("auth: cannot fetch token: {e}"))?;
        read_two_legged(response)
            .await
            .map(|token| (token.access_token, token.expiry))
    }
}

#[cfg(test)]
mod tests {
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

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

        let jwt = flow.assertion(1_700_000_000).unwrap();
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
}
