use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::DateTime;
use reqwest::Client;
use serde::{Deserialize, Serialize};

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const DEFAULT_LIFETIME: &str = "3600s";
const GO_VERSION: &str = "1.26.8";

pub struct Exchange<'a> {
    pub client: &'a Client,
    pub endpoint: &'a str,
    pub audience: &'a str,
    pub subject_token_type: &'a str,
    pub subject_token: &'a str,
    pub scopes: &'a [String],
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub workforce_user_project: &'a str,
    pub source: &'a str,
    pub impersonating: bool,
    pub lifetime_configured: bool,
}

#[derive(Deserialize)]
struct StsResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Serialize)]
struct GenerateAccessToken<'a> {
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    delegates: &'a [String],
    lifetime: String,
    scope: &'a [String],
}

#[derive(Deserialize)]
struct ImpersonationResponse {
    #[serde(default, rename = "accessToken")]
    access_token: String,
    #[serde(default, rename = "expireTime")]
    expire_time: String,
}

impl Exchange<'_> {
    pub async fn token(&self) -> Result<(String, SystemTime), String> {
        let mut form = vec![
            ("audience", self.audience.to_owned()),
            ("grant_type", GRANT_TYPE.to_owned()),
            ("requested_token_type", TOKEN_TYPE.to_owned()),
            ("subject_token_type", self.subject_token_type.to_owned()),
            ("subject_token", self.subject_token.to_owned()),
            ("scope", self.scopes.join(" ")),
        ];
        if !self.workforce_user_project.is_empty() && self.client_id.is_empty() {
            form.push((
                "options",
                serde_json::json!({ "userProject": self.workforce_user_project }).to_string(),
            ));
        }
        let mut request = self
            .client
            .post(self.endpoint)
            .header(
                "x-goog-api-client",
                format!(
                    "gl-go/{GO_VERSION} auth/unknown google-byoid-sdk source/{} sa-impersonation/{} config-lifetime/{}",
                    self.source, self.impersonating, self.lifetime_configured
                ),
            )
            .form(&form);
        if !self.client_id.is_empty() && !self.client_secret.is_empty() {
            let basic = STANDARD.encode(format!("{}:{}", self.client_id, self.client_secret));
            request = request.header("Authorization", format!("Basic {basic}"));
        }
        let parsed = send(request).await?;
        match parsed.expires_in {
            seconds if seconds <= 0 => Err("credentials: got invalid expiry from security token service".to_owned()),
            seconds => Ok((
                parsed.access_token,
                SystemTime::now() + Duration::from_secs(u64::try_from(seconds).unwrap_or(0)),
            )),
        }
    }
}

async fn send(request: reqwest::RequestBuilder) -> Result<StsResponse, String> {
    let response = request
        .send()
        .await
        .map_err(|e| format!("credentials: invalid response from Secure Token Server: {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|e| format!("credentials: invalid response from Secure Token Server: {e}"))?;
    if !(200..=300).contains(&status) {
        return Err(format!("credentials: status code {status}: {body}"));
    }
    serde_json::from_str(&body)
        .map_err(|e| format!("credentials: failed to unmarshal response body from Secure Token Server: {e}"))
}

pub struct Refreshed {
    pub access_token: String,
    pub refresh_token: String,
    pub expiry: SystemTime,
}

pub async fn refresh(
    client: &Client,
    endpoint: &str,
    refresh_token: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<Refreshed, String> {
    let basic = STANDARD.encode(format!("{client_id}:{client_secret}"));
    let request = client
        .post(endpoint)
        .header("Authorization", format!("Basic {basic}"))
        .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token)]);
    let parsed = send(request).await?;
    match u64::try_from(parsed.expires_in) {
        Err(_) => Err("credentials: invalid expiry from security token service".to_owned()),
        Ok(seconds) => Ok(Refreshed {
            access_token: parsed.access_token,
            refresh_token: parsed.refresh_token,
            expiry: SystemTime::now() + Duration::from_secs(seconds),
        }),
    }
}

pub async fn impersonate(
    client: &Client,
    url: &str,
    source_token: &str,
    scopes: &[String],
    delegates: &[String],
    lifetime_seconds: i64,
) -> Result<(String, SystemTime), String> {
    let lifetime = match lifetime_seconds {
        0 => DEFAULT_LIFETIME.to_owned(),
        seconds => format!("{seconds}s"),
    };
    let body = serde_json::to_string(&GenerateAccessToken {
        delegates,
        lifetime,
        scope: scopes,
    })
    .map_err(|e| format!("credentials: unable to marshal request: {e}"))?;
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .bearer_auth(source_token)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("credentials: unable to generate access token: {e}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| format!("credentials: unable to generate access token: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("credentials: status code {status}: {text}"));
    }
    let parsed: ImpersonationResponse =
        serde_json::from_str(&text).map_err(|e| format!("credentials: unable to parse response: {e}"))?;
    let expiry = DateTime::parse_from_rfc3339(&parsed.expire_time)
        .map_err(|e| format!("credentials: unable to parse expiry: {e}"))?;
    Ok((parsed.access_token, SystemTime::from(expiry)))
}
