mod endpoints;
mod region;
mod token_response;

use reqwest::Client;

use super::azcore::{CallError, HttpFailure};
use super::tenant::{TenantPolicy, valid_tenant};
use super::token_source::SourceError;
use crate::go_url::GoUrl;
pub use endpoints::Endpoints;
use endpoints::Resolver;
use region::Region;
pub use token_response::Grant;

const AUTHORITY_HOST_ENV: &str = "AZURE_AUTHORITY_HOST";
const REGIONAL_AUTHORITY_ENV: &str = "AZURE_REGIONAL_AUTHORITY_NAME";
const FORCE_REGION_ENV: &str = "MSAL_FORCE_REGION";
const PUBLIC_CLOUD: &str = "https://login.microsoftonline.com";
const DEFAULT_SCOPES: [&str; 3] = ["openid", "offline_access", "profile"];
const CLIENT_CAPABILITIES: &str = r#"{"access_token":{"xms_cc":{"values":["CP1"]}}}"#;
const NON_SPECIFIC_TENANTS: [&str; 3] = ["common", "consumers", "organizations"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Confidential,
    Public,
}

#[derive(Clone)]
pub struct Authority {
    resolver: Resolver,
}

pub struct Exchange<'a> {
    pub credential: &'a str,
    pub scope: &'a str,
    pub cae: bool,
    pub form: Vec<(&'static str, String)>,
}

impl Authority {
    pub fn for_tenant(tenant: &str, kind: ClientKind) -> Result<Self, String> {
        valid_tenant(tenant)?;
        let origin = authority_host()?.trim_end_matches('/').to_owned();
        Resolver::new(&origin, tenant, region_for(kind)).map(|resolver| Self { resolver })
    }

    #[cfg(test)]
    pub fn local(base_url: &str) -> Self {
        let mut resolver = Resolver::new(base_url, "t", Region::Global).unwrap_or_else(|e| panic!("{e}"));
        resolver.validate = false;
        Self { resolver }
    }

    #[cfg(test)]
    pub fn local_adfs(base_url: &str) -> Self {
        let mut resolver = Resolver::new(base_url, "adfs", Region::Global).unwrap_or_else(|e| panic!("{e}"));
        resolver.validate = false;
        Self { resolver }
    }

    pub fn is_adfs(&self) -> bool {
        self.resolver.adfs
    }

    pub fn tenant(&self) -> &str {
        &self.resolver.tenant
    }

    pub fn for_request(&self, tenants: &TenantPolicy, credential: &str, specified: &str) -> Result<Self, SourceError> {
        let tenant = tenants.resolve(credential, specified).map_err(SourceError::Failed)?;
        self.with_tenant(&tenant)
            .map_err(|e| SourceError::Failed(format!("{credential}: {e}")))
    }

    pub fn with_tenant(&self, tenant: &str) -> Result<Self, String> {
        if tenant.is_empty() || tenant == self.resolver.tenant {
            return Ok(self.clone());
        }
        if self.resolver.adfs {
            return Err("ADFS authority doesn't support tenants".to_owned());
        }
        if NON_SPECIFIC_TENANTS.contains(&tenant) {
            return Err(format!("tenant ID must be a specific tenant, not \"{tenant}\""));
        }
        if self.resolver.tenant.eq_ignore_ascii_case("consumers") {
            return Err(
                "client is configured to authenticate only personal Microsoft accounts, via the \"consumers\" endpoint"
                    .to_owned(),
            );
        }
        Ok(Self {
            resolver: self.resolver.for_tenant(tenant),
        })
    }

    pub async fn resolve(&self, client: &Client) -> Result<Endpoints, CallError> {
        self.resolver.resolve(client).await
    }
}

fn region_for(kind: ClientKind) -> Region {
    match kind {
        ClientKind::Public => Region::Global,
        ClientKind::Confidential => Region::configured(
            &std::env::var(REGIONAL_AUTHORITY_ENV).unwrap_or_default(),
            &std::env::var(FORCE_REGION_ENV).unwrap_or_default(),
        ),
    }
}

impl Endpoints {
    pub async fn exchange(&self, client: &Client, exchange: Exchange<'_>) -> Result<Grant, SourceError> {
        let credential = exchange.credential;
        let failure = |message: String| SourceError::Failed(format!("{credential}: {message}"));
        let response = client
            .post(&self.token_endpoint)
            .header("Accept", "application/json")
            .form(&form_of(&exchange))
            .send()
            .await
            .map_err(|e| failure(format!("server response error:\n {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| failure(format!("could not read the body of an HTTP Response: {e}")))?;
        match status.as_u16() {
            200 | 201 => token_response::parse(&body, exchange.scope).map_err(failure),
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

fn form_of(exchange: &Exchange<'_>) -> Vec<(&'static str, String)> {
    let mut form = exchange.form.clone();
    form.push(("scope", scopes(exchange.scope)));
    if exchange.cae {
        form.push(("claims", CLIENT_CAPABILITIES.to_owned()));
    }
    form.sort_by_key(|(key, _)| *key);
    form
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
    fn tenants_are_validated_like_the_sdk() {
        assert!(valid_tenant("contoso.onmicrosoft.com").is_ok());
        assert_eq!(valid_tenant("a/b").unwrap_err(), super::super::tenant::INVALID_TENANT);
        assert!(Authority::for_tenant("a/b", ClientKind::Public).is_err());
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

    #[test]
    fn other_tenants_are_scoped_through_the_policy() {
        let authority = Authority::for_tenant("home", ClientKind::Confidential).unwrap();
        let policy = TenantPolicy::new("home", &["other".to_owned()]);

        assert_eq!(authority.for_request(&policy, "C", "").unwrap().tenant(), "home");
        assert_eq!(authority.for_request(&policy, "C", "other").unwrap().tenant(), "other");
        assert_eq!(
            authority.for_request(&policy, "C", "third").err().unwrap(),
            SourceError::Failed("C isn't configured to acquire tokens for tenant \"third\". To enable acquiring tokens for this tenant add it to the AdditionallyAllowedTenants on the credential options, or add \"*\" to allow acquiring tokens for any tenant".to_owned())
        );
        let wildcard = TenantPolicy::new("home", &["*".to_owned()]);
        assert_eq!(
            authority.for_request(&wildcard, "C", "common").err().unwrap(),
            SourceError::Failed("C: tenant ID must be a specific tenant, not \"common\"".to_owned())
        );
    }

    #[tokio::test]
    async fn exchanges_send_sorted_forms_with_the_cae_capability() {
        let (base, handle) = super::super::testing::serve(vec![
            (
                200,
                "application/json",
                r#"{"access_token":"a","expires_in":3600}"#.to_owned(),
            ),
            (
                200,
                "application/json",
                r#"{"access_token":"b","expires_in":3600}"#.to_owned(),
            ),
        ])
        .await;
        let endpoints = Endpoints {
            authorization_endpoint: String::new(),
            token_endpoint: format!("{base}/token"),
            origin: String::new(),
        };
        let form = || vec![("grant_type", "g".to_owned()), ("client_id", "c".to_owned())];

        endpoints
            .exchange(
                &Client::new(),
                Exchange {
                    credential: "C",
                    scope: "s",
                    cae: true,
                    form: form(),
                },
            )
            .await
            .unwrap();
        endpoints
            .exchange(
                &Client::new(),
                Exchange {
                    credential: "C",
                    scope: "s",
                    cae: false,
                    form: form(),
                },
            )
            .await
            .unwrap();
        let requests = handle.await.unwrap();

        assert!(requests[0].ends_with(
            "claims=%7B%22access_token%22%3A%7B%22xms_cc%22%3A%7B%22values%22%3A%5B%22CP1%22%5D%7D%7D%7D&client_id=c&grant_type=g&scope=s+openid+offline_access+profile"
        ));
        assert!(!requests[1].contains("claims="));
    }
}
