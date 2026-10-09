use std::collections::HashMap;
use std::sync::Arc;

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::super::azcore::{CallError, HttpFailure};
use super::region::Region;
use crate::go_url::GoUrl;

const DEFAULT_HOST: &str = "login.microsoftonline.com";
const LOGIN_MICROSOFT: &str = "login.microsoft.com";
const LOGIN_WINDOWS: &str = "login.windows.net";
const LOGIN_STS_WINDOWS: &str = "sts.windows.net";
const TRUSTED_HOSTS: [&str; 10] = [
    "login.windows.net",
    "login.partner.microsoftonline.cn",
    "login.chinacloudapi.cn",
    "login.microsoftonline.de",
    "login-us.microsoftonline.com",
    "login.microsoftonline.us",
    "login.microsoftonline.com",
    "login.microsoft.com",
    "sts.windows.net",
    "login.usgovcloudapi.net",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub origin: String,
}

#[derive(Clone, Default)]
pub struct Overrides {
    pub instance_discovery_origin: Option<String>,
    pub region_probe_url: Option<String>,
}

#[derive(Clone)]
pub struct Resolver {
    pub origin: String,
    pub host: String,
    pub tenant: String,
    pub adfs: bool,
    pub validate: bool,
    pub region: Region,
    pub overrides: Overrides,
    cache: Arc<Mutex<HashMap<String, Endpoints>>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OpenIdConfiguration {
    authorization_endpoint: String,
    token_endpoint: String,
    issuer: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct InstanceDiscovery {
    tenant_discovery_endpoint: String,
}

pub fn trusted_host(host: &str) -> bool {
    TRUSTED_HOSTS.contains(&host)
}

impl OpenIdConfiguration {
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

    fn validate_issuer(&self, authority: &str) -> Result<(), String> {
        let issuer = GoUrl::parse(&self.issuer)
            .map_err(|e| format!("TenantDiscoveryResponse: failed to parse issuer URL: {e}"))?;
        let parsed = GoUrl::parse(authority)
            .map_err(|e| format!("TenantDiscoveryResponse: failed to parse authority URL: {e}"))?;
        let accepted = (issuer.scheme == parsed.scheme && issuer.host == parsed.host)
            || trusted_host(&issuer.host)
            || parsed.host.ends_with(&format!(".{}", issuer.host));
        match accepted {
            true => Ok(()),
            false => Err(format!(
                "TenantDiscoveryResponse: issuer '{}' does not match authority '{authority}' or any trusted/alias rule",
                self.issuer
            )),
        }
    }
}

impl Resolver {
    pub fn new(origin: &str, tenant: &str, region: Region) -> Result<Self, String> {
        let url = GoUrl::parse(origin).map_err(|e| format!("couldn't parse authority url: {e}"))?;
        Ok(Self {
            origin: origin.to_owned(),
            host: url.host,
            tenant: tenant.to_owned(),
            adfs: tenant.eq_ignore_ascii_case("adfs"),
            validate: !tenant.eq_ignore_ascii_case("adfs"),
            region,
            overrides: Overrides::default(),
            cache: Arc::default(),
        })
    }

    pub fn canonical(&self) -> String {
        format!("{}/{}/", self.origin, self.tenant)
    }

    pub fn for_tenant(&self, tenant: &str) -> Self {
        Self {
            tenant: tenant.to_owned(),
            adfs: tenant.eq_ignore_ascii_case("adfs"),
            ..self.clone()
        }
    }

    pub async fn resolve(&self, client: &Client) -> Result<Endpoints, CallError> {
        let wrap = |message: String| CallError::Message(format!("unable to resolve an endpoint: {message}"));
        if let Some(found) = self.cache.lock().await.get(&self.canonical()) {
            return Ok(found.clone());
        }
        let url = self.configuration_url(client).await.map_err(|e| match e {
            CallError::Message(message) => wrap(message),
            http => http,
        })?;
        let configuration: OpenIdConfiguration = get_json(client, &url).await.map_err(|e| match e {
            CallError::Message(message) => wrap(message),
            http => http,
        })?;
        configuration
            .validate()
            .map_err(|e| wrap(format!("ResolveEndpoints(): {e}")))?;
        let tenant = self.tenant.to_lowercase();
        let endpoints = Endpoints {
            authorization_endpoint: configuration.authorization_endpoint.replace("{tenant}", &tenant),
            token_endpoint: configuration.token_endpoint.replace("{tenant}", &tenant),
            origin: self.origin.clone(),
        };
        self.cache.lock().await.insert(self.canonical(), endpoints.clone());
        configuration
            .validate_issuer(&self.canonical())
            .map_err(|e| wrap(format!("ResolveEndpoints(): {e}")))?;
        Ok(endpoints)
    }

    async fn configuration_url(&self, client: &Client) -> Result<String, CallError> {
        let discovers = (self.validate && !trusted_host(&self.host)) || !self.region.is_global();
        match (self.adfs, discovers) {
            (true, _) => Ok(format!("{}/adfs/.well-known/openid-configuration", self.origin)),
            (false, true) => self.instance_discovery(client).await,
            (false, false) => Ok(format!("{}v2.0/.well-known/openid-configuration", self.canonical())),
        }
    }

    async fn instance_discovery(&self, client: &Client) -> Result<String, CallError> {
        match self
            .region
            .resolved(client, self.overrides.region_probe_url.as_deref())
            .await
        {
            Some(region) => Ok(self.regional_configuration_url(&region)),
            None => self.remote_instance_discovery(client).await,
        }
    }

    fn regional_configuration_url(&self, region: &str) -> String {
        let environment = match self.host.as_str() {
            LOGIN_MICROSOFT | LOGIN_WINDOWS | LOGIN_STS_WINDOWS | DEFAULT_HOST => LOGIN_MICROSOFT,
            other => other,
        };
        format!(
            "https://{region}.{environment}/{}/v2.0/.well-known/openid-configuration",
            self.tenant.to_lowercase()
        )
    }

    async fn remote_instance_discovery(&self, client: &Client) -> Result<String, CallError> {
        let origin = self.overrides.instance_discovery_origin.clone().unwrap_or_else(|| {
            let host = match trusted_host(&self.host) {
                true => self.host.as_str(),
                false => DEFAULT_HOST,
            };
            format!("https://{host}")
        });
        let authorization_endpoint = format!(
            "https://{}/{}/oauth2/v2.0/authorize",
            self.host,
            self.tenant.to_lowercase()
        );
        let url = format!(
            "{origin}/common/discovery/instance?api-version=1.1&authorization_endpoint={}",
            crate::go_url::escape::escape(&authorization_endpoint, crate::go_url::escape::Mode::QueryComponent)
        );
        let discovery: InstanceDiscovery = get_json(client, &url).await?;
        Ok(discovery.tenant_discovery_endpoint)
    }
}

pub async fn get_json<T: serde::de::DeserializeOwned>(client: &Client, url: &str) -> Result<T, CallError> {
    if url.is_empty() {
        return Err(CallError::Message(
            "server response error:\n Get \"\": unsupported protocol scheme \"\"".to_owned(),
        ));
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| CallError::Message(format!("server response error:\n {e}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| CallError::Message(format!("could not read the body of an HTTP Response: {e}")))?;
    match status {
        StatusCode::OK | StatusCode::CREATED => serde_json::from_str(&body)
            .map_err(|e| CallError::Message(format!("json decode error: {e}\njson message bytes were: {body}"))),
        _ => Err(CallError::Http(HttpFailure {
            method: "GET".to_owned(),
            url: url.to_owned(),
            status,
            body,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::testing::serve;
    use super::*;

    fn resolver(base: &str) -> Resolver {
        let mut resolver = Resolver::new(base, "t", Region::Global).unwrap();
        resolver.validate = false;
        resolver
    }

    fn configuration(token: &str, issuer: &str) -> (u16, &'static str, String) {
        (
            200,
            "application/json",
            format!(
                r#"{{"authorization_endpoint":"{{BASE}}/{{tenant}}/authorize","token_endpoint":"{token}","issuer":"{issuer}"}}"#
            ),
        )
    }

    #[tokio::test]
    async fn the_openid_configuration_supplies_the_endpoints_once() {
        let (base, handle) = serve(vec![configuration("{BASE}/{tenant}/token", "{BASE}/{tenant}/v2.0")]).await;
        let resolver = resolver(&base);

        let first = resolver.resolve(&Client::new()).await.unwrap();
        let second = resolver.resolve(&Client::new()).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first.token_endpoint, format!("{base}/t/token"));
        assert_eq!(first.authorization_endpoint, format!("{base}/t/authorize"));
        let requests = handle.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /t/v2.0/.well-known/openid-configuration HTTP/1.1"));
    }

    #[tokio::test]
    async fn incomplete_configurations_name_the_missing_field() {
        let (base, _handle) = serve(vec![(200, "application/json", r#"{"issuer":"x"}"#.to_owned())]).await;

        let error = resolver(&base).resolve(&Client::new()).await.err().unwrap();

        assert_eq!(
            error.message(),
            "unable to resolve an endpoint: ResolveEndpoints(): TenantDiscoveryResponse: authorize endpoint was not found in the openid configuration"
        );
    }

    #[tokio::test]
    async fn a_foreign_issuer_is_rejected_but_the_endpoints_stay_cached_like_msal() {
        let (base, _handle) = serve(vec![configuration("{BASE}/token", "https://evil.example/t")]).await;
        let resolver = resolver(&base);

        let rejected = resolver.resolve(&Client::new()).await.err().unwrap();
        let cached = resolver.resolve(&Client::new()).await;

        assert!(rejected.message().starts_with(
            "unable to resolve an endpoint: ResolveEndpoints(): TenantDiscoveryResponse: issuer 'https://evil.example/t' does not match authority"
        ));
        assert!(cached.is_ok());
    }

    #[tokio::test]
    async fn untrusted_hosts_ask_the_instance_discovery_endpoint_first() {
        let (base, handle) = serve(vec![configuration("{BASE}/token", "{BASE}/t")]).await;
        let (discovery, discovery_handle) = serve(vec![(
            200,
            "application/json",
            format!(r#"{{"tenant_discovery_endpoint":"{base}/t/v2.0/.well-known/openid-configuration"}}"#),
        )])
        .await;
        let mut resolver = Resolver::new(&base, "t", Region::Global).unwrap();
        resolver.overrides.instance_discovery_origin = Some(discovery);

        let endpoints = resolver.resolve(&Client::new()).await.unwrap();

        let asked = discovery_handle.await.unwrap();
        assert!(asked[0].starts_with(
            "GET /common/discovery/instance?api-version=1.1&authorization_endpoint=https%3A%2F%2F127.0.0.1"
        ));
        assert!(asked[0].contains("%2Ft%2Foauth2%2Fv2.0%2Fauthorize"));
        assert_eq!(endpoints.token_endpoint, format!("{base}/token"));
        assert!(handle.await.unwrap()[0].starts_with("GET /t/v2.0/.well-known/openid-configuration"));
    }

    #[test]
    fn regional_endpoints_use_the_region_prefixed_host() {
        let resolver = Resolver::new(
            "https://login.microsoftonline.com",
            "T",
            Region::Named("westus2".to_owned()),
        )
        .unwrap();

        assert_eq!(
            resolver.regional_configuration_url("westus2"),
            "https://westus2.login.microsoft.com/t/v2.0/.well-known/openid-configuration"
        );
        let sovereign = Resolver::new("https://login.microsoftonline.us", "t", Region::Global).unwrap();
        assert_eq!(
            sovereign.regional_configuration_url("usgovvirginia"),
            "https://usgovvirginia.login.microsoftonline.us/t/v2.0/.well-known/openid-configuration"
        );
    }

    #[tokio::test]
    async fn adfs_authorities_use_the_adfs_configuration_document() {
        let (base, handle) = serve(vec![configuration("{BASE}/adfs/oauth2/token", "{BASE}/adfs")]).await;
        let resolver = Resolver::new(&base, "adfs", Region::Global).unwrap();

        let endpoints = resolver.resolve(&Client::new()).await.unwrap();

        assert_eq!(endpoints.token_endpoint, format!("{base}/adfs/oauth2/token"));
        assert!(handle.await.unwrap()[0].starts_with("GET /adfs/.well-known/openid-configuration HTTP/1.1"));
    }
}
