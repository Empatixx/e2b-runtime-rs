use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use rsa::RsaPrivateKey;
use serde::Deserialize;

use super::super::flows::Dialect;
use super::super::flows::{FlowToken, TokenFlow, default_client, request_error};
use super::super::form::UrlEncodedForm;
use super::super::key_errors::oauth2_rsa_key;
use super::super::private_key::sign_rsa;
use super::super::{credsfile, json};
use crate::storage::backend::pem;

const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const BACKDATE_SECONDS: u64 = 10;
const LIFETIME_SECONDS: u64 = 3600;

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServiceAccountFile {
    client_email: String,
    private_key: String,
    private_key_id: String,
    token_uri: String,
    audience: String,
}

pub struct JwtFlow {
    file: ServiceAccountFile,
    scopes: Vec<String>,
    client: Client,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
}

impl JwtFlow {
    pub fn from_json(json: &[u8], scopes: &[String]) -> Result<Arc<Self>, String> {
        let file: ServiceAccountFile = json::unmarshal(json, credsfile::OAUTH2_CREDENTIALS_FILE)?;
        Ok(Arc::new(Self {
            file,
            scopes: scopes.to_vec(),
            client: default_client()?,
        }))
    }

    fn token_url(&self) -> &str {
        match self.file.token_uri.is_empty() {
            true => DEFAULT_TOKEN_URL,
            false => &self.file.token_uri,
        }
    }

    fn audience(&self) -> &str {
        match self.file.audience.is_empty() {
            true => self.token_url(),
            false => &self.file.audience,
        }
    }

    fn assertion(&self, now_unix: u64) -> Result<String, String> {
        let key = parse_key(&self.file.private_key)?;
        let issued = now_unix - BACKDATE_SECONDS;
        let header = format!(
            r#"{{"alg":"RS256","typ":"JWT"{}}}"#,
            match self.file.private_key_id.is_empty() {
                true => String::new(),
                false => format!(r#","kid":{}"#, json(&self.file.private_key_id)),
            }
        );
        let scope = match self.scopes.is_empty() {
            true => String::new(),
            false => format!(r#""scope":{},"#, json(&self.scopes.join(" "))),
        };
        let claims = format!(
            r#"{{"iss":{},{scope}"aud":{},"exp":{},"iat":{issued}}}"#,
            json(&self.file.client_email),
            json(self.audience()),
            issued + LIFETIME_SECONDS
        );
        let signing_input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(claims));
        let signature = sign_rsa(&key, signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

fn parse_key(private_key: &str) -> Result<RsaPrivateKey, String> {
    oauth2_rsa_key(&pem::first_der_or_raw(private_key))
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
impl TokenFlow for JwtFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        let assertion = self.assertion(now_unix())?;
        let response = self
            .client
            .post(self.token_url())
            .url_encoded_form([("grant_type", GRANT_TYPE), ("assertion", assertion.as_str())])
            .send()
            .await
            .map_err(|e| {
                format!(
                    "oauth2: cannot fetch token: {}",
                    request_error("Post", self.token_url(), &e)
                )
            })?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("oauth2: cannot fetch token: {e}"))?;
        if !status.is_success() {
            return Err(Dialect::Oauth2.cannot_fetch(status, &body));
        }
        let parsed: TokenResponse = json::unmarshal(body.as_bytes(), credsfile::OAUTH2_JWT_RESPONSE)
            .map_err(|e| format!("oauth2: cannot fetch token: {e}"))?;
        Ok(FlowToken {
            value: parsed.access_token,
            expiry: u64::try_from(parsed.expires_in)
                .ok()
                .filter(|seconds| *seconds > 0)
                .map(|seconds| SystemTime::now() + std::time::Duration::from_secs(seconds)),
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::storage::backend::azure::testing::read_request;

    const KEY: &str = include_str!("../test_key.pem");

    fn flow(extra: serde_json::Value) -> Arc<JwtFlow> {
        let mut file = serde_json::json!({"client_email": "sa@p.iam", "private_key": KEY, "private_key_id": "kid1"});
        file.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        JwtFlow::from_json(
            file.to_string().as_bytes(),
            &["scope-a".to_owned(), "scope-b".to_owned()],
        )
        .unwrap()
    }

    fn decode(part: &str) -> String {
        String::from_utf8(URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn the_assertion_claims_follow_x_oauth2_jws() {
        let jwt = flow(serde_json::json!({"token_uri": "https://tokens/x"}))
            .assertion(1_700_000_000)
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();

        assert_eq!(decode(parts[0]), r#"{"alg":"RS256","typ":"JWT","kid":"kid1"}"#);
        assert_eq!(
            decode(parts[1]),
            r#"{"iss":"sa@p.iam","scope":"scope-a scope-b","aud":"https://tokens/x","exp":1700003590,"iat":1699999990}"#
        );
    }

    #[test]
    fn an_explicit_audience_replaces_the_token_url_and_missing_ids_are_omitted() {
        let file = serde_json::json!({"client_email": "sa@p.iam", "private_key": KEY, "audience": "https://aud"});

        let jwt = JwtFlow::from_json(file.to_string().as_bytes(), &[])
            .unwrap()
            .assertion(1_700_000_000)
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();

        assert_eq!(decode(parts[0]), r#"{"alg":"RS256","typ":"JWT"}"#);
        assert_eq!(
            decode(parts[1]),
            r#"{"iss":"sa@p.iam","aud":"https://aud","exp":1700003590,"iat":1699999990}"#
        );
    }

    #[test]
    fn unreadable_keys_use_the_x_oauth2_wording() {
        let file = serde_json::json!({"client_email": "a@b", "private_key": "nope"});
        let flow = JwtFlow::from_json(file.to_string().as_bytes(), &[]).unwrap();

        assert!(
            flow.assertion(1_700_000_000)
                .unwrap_err()
                .starts_with("private key should be a PEM or plain PKCS1 or PKCS8; parse error: asn1: structure error")
        );
    }

    async fn serve(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_request(&mut connection).await;
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}/token"), handle)
    }

    #[tokio::test]
    async fn the_exchange_posts_a_sorted_form_and_reads_the_expiry() {
        let (url, server) = serve(200, r#"{"access_token":"at","expires_in":3600}"#).await;

        let token = flow(serde_json::json!({"token_uri": url})).token().await.unwrap();
        let request = server.await.unwrap();

        assert_eq!(token.value, "at");
        assert!(token.expiry.is_some());
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("\r\n\r\nassertion="));
        assert!(request.contains("&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"));
    }

    #[tokio::test]
    async fn failures_carry_the_status_line_and_body() {
        let (url, _server) = serve(400, r#"{"error":"invalid_grant"}"#).await;

        assert_eq!(
            flow(serde_json::json!({"token_uri": url})).token().await.unwrap_err(),
            "oauth2: cannot fetch token: 400 Bad Request\nResponse: {\"error\":\"invalid_grant\"}"
        );
    }
}
