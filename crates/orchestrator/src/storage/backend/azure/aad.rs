use std::time::{Duration, SystemTime};

use reqwest::Client;
use serde::Deserialize;

use super::azcore::indent_json;
use super::token_source::{BearerToken, SourceError};

const AUTHORITY_HOST_ENV: &str = "AZURE_AUTHORITY_HOST";
const PUBLIC_CLOUD: &str = "https://login.microsoftonline.com";
const DEFAULT_SCOPES: [&str; 3] = ["openid", "offline_access", "profile"];
const SEPARATOR: &str = "--------------------------------------------------------------------------------";
const TROUBLESHOOT: &str = "To troubleshoot, visit https://aka.ms/azsdk/go/identity/troubleshoot";

#[derive(Clone)]
pub struct Authority {
    base_url: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default, deserialize_with = "seconds")]
    expires_in: u64,
}

fn seconds<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Integer(u64),
        Text(String),
    }
    Ok(match Number::deserialize(deserializer)? {
        Number::Integer(value) => value,
        Number::Text(text) => text.parse().unwrap_or(0),
    })
}

impl Authority {
    pub fn for_tenant(tenant: &str) -> Self {
        let host = std::env::var(AUTHORITY_HOST_ENV)
            .ok()
            .filter(|host| !host.is_empty())
            .unwrap_or_else(|| PUBLIC_CLOUD.to_owned());
        let base_url = host.trim_end_matches('/').to_owned();
        Self {
            token_endpoint: format!("{base_url}/{tenant}/oauth2/v2.0/token"),
            base_url,
        }
    }

    #[cfg(test)]
    pub fn local(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_owned(),
            token_endpoint: format!("{base_url}/t/oauth2/v2.0/token"),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn token_endpoint(&self) -> &str {
        &self.token_endpoint
    }

    pub async fn request_token(
        &self,
        client: &Client,
        credential: &str,
        anchor: &str,
        scope: &str,
        mut form: Vec<(&str, String)>,
    ) -> Result<BearerToken, SourceError> {
        form.push(("scope", scopes(scope)));
        let failure = |message: String| SourceError::Failed(format!("{credential}: {message}"));
        let response = client
            .post(&self.token_endpoint)
            .header("Accept", "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|e| failure(e.to_string()))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| failure(e.to_string()))?;
        match status.as_u16() {
            200 => parse(&body).map_err(failure),
            code => Err(SourceError::Failed(rejection(
                credential,
                anchor,
                &self.token_endpoint,
                (code, &status.to_string()),
                &body,
            ))),
        }
    }
}

fn scopes(scope: &str) -> String {
    scope
        .split_whitespace()
        .filter(|scope| !DEFAULT_SCOPES.contains(scope))
        .chain(DEFAULT_SCOPES)
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse(body: &str) -> Result<BearerToken, String> {
    let response: TokenResponse = serde_json::from_str(body).map_err(|e| e.to_string())?;
    match response.access_token.is_empty() {
        true => Err("response is missing an access token".to_owned()),
        false => Ok(BearerToken {
            secret: response.access_token,
            expires_at: SystemTime::now() + Duration::from_secs(response.expires_in),
        }),
    }
}

fn rejection(credential: &str, anchor: &str, endpoint: &str, status: (u16, &str), body: &str) -> String {
    let shown = match (body.is_empty(), serde_json::from_str::<serde_json::Value>(body).is_ok()) {
        (true, _) => "Response contained no body".to_owned(),
        (false, true) => indent_json(body),
        (false, false) => body.to_owned(),
    };
    let advice = match anchor.is_empty() {
        true => String::new(),
        false => format!("{TROUBLESHOOT}#{anchor}"),
    };
    format!(
        "{credential} authentication failed. \nPOST {endpoint}\n{SEPARATOR}\nRESPONSE {}: {}\n{SEPARATOR}\n{shown}\n{SEPARATOR}\n{advice}",
        status.0, status.1
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_scopes_are_appended_once() {
        assert_eq!(
            scopes("https://x/.default"),
            "https://x/.default openid offline_access profile"
        );
        assert_eq!(
            scopes("https://x/.default openid"),
            "https://x/.default openid offline_access profile"
        );
    }

    #[test]
    fn rejections_read_like_the_sdk() {
        let text = rejection(
            "ClientCertificateCredential",
            "client-cert",
            "https://login/t/oauth2/v2.0/token",
            (401, "401 Unauthorized"),
            r#"{"error":"invalid_client"}"#,
        );

        assert_eq!(
            text,
            format!(
                "ClientCertificateCredential authentication failed. \nPOST https://login/t/oauth2/v2.0/token\n{SEPARATOR}\nRESPONSE 401: 401 Unauthorized\n{SEPARATOR}\n{{\n  \"error\": \"invalid_client\"\n}}\n{SEPARATOR}\n{TROUBLESHOOT}#client-cert"
            )
        );
    }

    #[test]
    fn token_responses_accept_numeric_and_textual_lifetimes() {
        assert!(parse(r#"{"access_token":"a","expires_in":"3600"}"#).unwrap().expires_at > SystemTime::now());
        assert_eq!(parse("{}").unwrap_err(), "response is missing an access token");
    }
}
