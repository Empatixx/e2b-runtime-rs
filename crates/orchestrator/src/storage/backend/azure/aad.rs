use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::OnceCell;

use super::azcore::{CallError, HttpFailure};
use super::token_source::{BearerToken, SourceError};
use crate::go_url::GoUrl;

const AUTHORITY_HOST_ENV: &str = "AZURE_AUTHORITY_HOST";
const PUBLIC_CLOUD: &str = "https://login.microsoftonline.com";
const DEFAULT_SCOPES: [&str; 3] = ["openid", "offline_access", "profile"];
const INVALID_TENANT: &str = "invalid tenantID. You can locate your tenantID by following the instructions listed here: https://learn.microsoft.com/partner-center/find-ids-and-domain-names";

#[derive(Clone)]
pub struct Authority {
    adfs: bool,
    base_url: String,
    token_endpoint: String,
    discovered: Arc<OnceCell<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    issuer: String,
}

impl Discovery {
    fn validate(&self) -> Result<(), String> {
        let missing = [
            (&self.authorization_endpoint, "authorize endpoint"),
            (&self.token_endpoint, "token endpoint"),
            (&self.issuer, "issuer"),
        ]
        .into_iter()
        .find(|(value, _)| value.is_empty());
        match missing {
            Some((_, name)) => Err(format!(
                "TenantDiscoveryResponse: {name} was not found in the openid configuration"
            )),
            None => Ok(()),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
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
    pub fn for_tenant(tenant: &str) -> Result<Self, String> {
        valid_tenant(tenant)?;
        let base_url = authority_host()?.trim_end_matches('/').to_owned();
        Ok(Self {
            adfs: tenant == "adfs",
            token_endpoint: format!("{base_url}/{tenant}/oauth2/v2.0/token"),
            base_url,
            discovered: Arc::default(),
        })
    }

    #[cfg(test)]
    pub fn local(base_url: &str) -> Self {
        Self {
            adfs: false,
            base_url: base_url.to_owned(),
            token_endpoint: format!("{base_url}/t/oauth2/v2.0/token"),
            discovered: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn local_adfs(base_url: &str) -> Self {
        Self {
            adfs: true,
            ..Self::local(base_url)
        }
    }

    pub fn is_adfs(&self) -> bool {
        self.adfs
    }

    pub async fn resolved(&self, client: &Client) -> Result<Self, CallError> {
        match self.adfs {
            false => Ok(self.clone()),
            true => self
                .discovered
                .get_or_try_init(|| self.discover_token_endpoint(client))
                .await
                .map(|token_endpoint| Self {
                    token_endpoint: token_endpoint.clone(),
                    ..self.clone()
                }),
        }
    }

    async fn discover_token_endpoint(&self, client: &Client) -> Result<String, CallError> {
        let wrap = |e: String| CallError::Message(format!("unable to resolve an endpoint: {e}"));
        let url = format!("{}/adfs/.well-known/openid-configuration", self.base_url);
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| wrap(format!("server response error:\n {e}")))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| wrap(e.to_string()))?;
        if !matches!(status.as_u16(), 200 | 201) {
            return Err(CallError::Http(HttpFailure {
                method: "GET".to_owned(),
                url,
                status,
                body,
            }));
        }
        let discovery: Discovery = serde_json::from_str(&body)
            .map_err(|e| wrap(format!("json decode error: {e}\njson message bytes were: {body}")))?;
        discovery
            .validate()
            .map_err(|e| wrap(format!("ResolveEndpoints(): {e}")))?;
        Ok(discovery.token_endpoint)
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
            .map_err(|e| failure(format!("server response error:\n {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| failure(format!("could not read the body of an HTTP Response: {e}")))?;
        match status.as_u16() {
            200 | 201 => parse(&body).map_err(failure),
            _ => Err(CallError::Http(HttpFailure {
                method: "POST".to_owned(),
                url: self.token_endpoint.clone(),
                status,
                body,
            })
            .failed(credential, |message| message)),
        }
    }
}

fn valid_tenant(tenant: &str) -> Result<(), String> {
    match !tenant.is_empty()
        && tenant
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        true => Ok(()),
        false => Err(INVALID_TENANT.to_owned()),
    }
}

fn authority_host() -> Result<String, String> {
    resolve_host(std::env::var(AUTHORITY_HOST_ENV).ok())
}

fn resolve_host(configured: Option<String>) -> Result<String, String> {
    let host = configured
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| PUBLIC_CLOUD.to_owned());
    let url = GoUrl::parse(&host).map_err(|e| format!("parse {}: {e}", crate::go_strconv::quote(&host)))?;
    match url.scheme == "https" {
        true => Ok(host),
        false => Err("cannot use an authority host without https".to_owned()),
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
    let response: TokenResponse =
        serde_json::from_str(body).map_err(|e| format!("json decode error: {e}\nraw message was: {body}"))?;
    match (response.error.is_empty(), response.access_token.is_empty()) {
        (false, _) => Err(format!("{}: {}", response.error, response.error_description)),
        (true, true) => Err("response is missing access_token".to_owned()),
        (true, false) => Ok(BearerToken {
            secret: response.access_token,
            expires_at: SystemTime::now() + Duration::from_secs(response.expires_in),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::azcore::AuthenticationFailure;
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
        let text = AuthenticationFailure {
            credential: "ClientCertificateCredential",
            message: "",
            method: "POST",
            url: "https://login/t/oauth2/v2.0/token",
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: r#"{"error":"invalid_client"}"#,
        }
        .text();

        assert_eq!(
            text,
            "ClientCertificateCredential authentication failed. \nPOST https://login/t/oauth2/v2.0/token\n--------------------------------------------------------------------------------\nRESPONSE 401: 401 Unauthorized\n--------------------------------------------------------------------------------\n{\n  \"error\": \"invalid_client\"\n}\n--------------------------------------------------------------------------------\nTo troubleshoot, visit https://aka.ms/azsdk/go/identity/troubleshoot#client-cert"
        );
    }

    #[test]
    fn token_responses_accept_numeric_and_textual_lifetimes() {
        assert!(parse(r#"{"access_token":"a","expires_in":"3600"}"#).unwrap().expires_at > SystemTime::now());
        assert_eq!(parse("{}").unwrap_err(), "response is missing access_token");
    }

    #[test]
    fn an_error_field_in_a_successful_response_is_the_failure() {
        assert_eq!(
            parse(r#"{"error":"invalid_grant","error_description":"expired"}"#).unwrap_err(),
            "invalid_grant: expired"
        );
    }

    #[test]
    fn tenants_are_validated_like_the_sdk() {
        assert!(valid_tenant("contoso.onmicrosoft.com").is_ok());
        assert!(valid_tenant("0000-aaaa").is_ok());
        assert_eq!(valid_tenant("").unwrap_err(), INVALID_TENANT);
        assert_eq!(valid_tenant("a/b").unwrap_err(), INVALID_TENANT);
        assert_eq!(valid_tenant("a b").unwrap_err(), INVALID_TENANT);
    }

    #[test]
    fn http_authority_hosts_are_rejected() {
        assert_eq!(
            resolve_host(Some("http://login.example".to_owned())).unwrap_err(),
            "cannot use an authority host without https"
        );
        assert_eq!(
            resolve_host(Some("https://login.example".to_owned())).unwrap(),
            "https://login.example"
        );
        assert_eq!(resolve_host(None).unwrap(), PUBLIC_CLOUD);
    }
}
