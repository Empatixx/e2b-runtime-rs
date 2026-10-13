use std::time::{Duration, SystemTime};

use reqwest::Client;
use tokio::sync::Mutex;

use super::aad::{Authority, ClientKind, Exchange};
use super::azcore;
use super::msal::{self, CacheKey, TokenCache};
use super::tenant::TenantPolicy;
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource};

const CREDENTIAL: &str = "WorkloadIdentityCredential";
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const ASSERTION_LIFETIME: Duration = Duration::from_secs(10 * 60);

pub struct WorkloadIdentityCredential {
    client_id: String,
    file: String,
    authority: Authority,
    tenants: TenantPolicy,
    client: Client,
    cache: Mutex<TokenCache>,
    assertion: Mutex<Assertion>,
}

struct Assertion {
    content: String,
    expires: SystemTime,
}

impl WorkloadIdentityCredential {
    pub fn from_environment(raw: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let client_id = raw("AZURE_CLIENT_ID").ok_or_else(|| {
            "no client ID specified. Check pod configuration or set ClientID in the options".to_owned()
        })?;
        let file = raw("AZURE_FEDERATED_TOKEN_FILE").ok_or_else(|| {
            "no token file specified. Check pod configuration or set TokenFilePath in the options".to_owned()
        })?;
        let tenant = raw("AZURE_TENANT_ID").ok_or_else(|| {
            "no tenant ID specified. Check pod configuration or set TenantID in the options".to_owned()
        })?;
        let tenants = TenantPolicy::from_environment(&tenant, raw);
        Ok(Self {
            client_id,
            file,
            authority: Authority::for_tenant(&tenant, ClientKind::Confidential)?,
            tenants,
            client: azcore::msal_client(),
            cache: Mutex::default(),
            assertion: Mutex::new(Assertion::expired()),
        })
    }

    async fn assertion(&self) -> Result<String, String> {
        let mut held = self.assertion.lock().await;
        if held.expires < SystemTime::now() {
            let content = std::fs::read(&self.file)
                .map_err(|e| crate::fs::path_error("open", std::path::Path::new(&self.file), &e))?;
            *held = Assertion {
                content: String::from_utf8_lossy(&content).into_owned(),
                expires: SystemTime::now() + ASSERTION_LIFETIME,
            };
        }
        Ok(held.content.clone())
    }

    async fn acquire(&self, authority: &Authority, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let endpoints = authority
            .resolve(&self.client)
            .await
            .map_err(|e| e.failed(CREDENTIAL, |message| message))?;
        let assertion = self
            .assertion()
            .await
            .map_err(|e| SourceError::Failed(format!("{CREDENTIAL}: {e}")))?;
        endpoints
            .exchange(
                &self.client,
                Exchange {
                    credential: CREDENTIAL,
                    scope: request.scope,
                    cae: request.cae,
                    form: vec![
                        ("grant_type", "client_credentials".to_owned()),
                        ("client_assertion_type", ASSERTION_TYPE.to_owned()),
                        ("client_assertion", assertion),
                        ("client_id", self.client_id.clone()),
                        ("client_info", "1".to_owned()),
                    ],
                },
            )
            .await
            .map(|grant| grant.token)
    }
}

impl Assertion {
    fn expired() -> Self {
        Self {
            content: String::new(),
            expires: SystemTime::UNIX_EPOCH,
        }
    }
}

#[async_trait::async_trait]
impl TokenSource for WorkloadIdentityCredential {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let authority = self.authority.for_request(&self.tenants, CREDENTIAL, request.tenant)?;
        let key = CacheKey::of(request, authority.tenant());
        let mut cache = self.cache.lock().await;
        msal::confidential_token(&mut cache, key, || self.acquire(&authority, request)).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::serve;
    use super::*;

    fn environment(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    fn credential(base: &str, file: &std::path::Path) -> WorkloadIdentityCredential {
        WorkloadIdentityCredential {
            client_id: "client".to_owned(),
            file: file.display().to_string(),
            authority: Authority::local(base),
            tenants: TenantPolicy::new("t", &[]),
            client: Client::new(),
            cache: Mutex::default(),
            assertion: Mutex::new(Assertion::expired()),
        }
    }

    #[test]
    fn missing_pod_configuration_is_reported_in_order() {
        let build = |pairs: &'static [(&'static str, &'static str)]| {
            WorkloadIdentityCredential::from_environment(&environment(pairs))
                .err()
                .unwrap()
        };

        assert_eq!(
            build(&[]),
            "no client ID specified. Check pod configuration or set ClientID in the options"
        );
        assert_eq!(
            build(&[("AZURE_CLIENT_ID", "c")]),
            "no token file specified. Check pod configuration or set TokenFilePath in the options"
        );
        assert_eq!(
            build(&[("AZURE_CLIENT_ID", "c"), ("AZURE_FEDERATED_TOKEN_FILE", "/f")]),
            "no tenant ID specified. Check pod configuration or set TenantID in the options"
        );
        assert!(
            build(&[
                ("AZURE_CLIENT_ID", "c"),
                ("AZURE_FEDERATED_TOKEN_FILE", "/f"),
                ("AZURE_TENANT_ID", "a b")
            ])
            .starts_with("invalid tenantID.")
        );
    }

    #[tokio::test]
    async fn the_token_file_is_the_client_assertion_and_is_reread_only_after_ten_minutes() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("token");
        std::fs::write(&file, "k8s-token\n").unwrap();
        let (base, handle) = serve(vec![
            (
                200,
                "application/json",
                r#"{"authorization_endpoint":"{BASE}/t/authorize","token_endpoint":"{BASE}/t/oauth2/v2.0/token","issuer":"{BASE}/t/v2.0"}"#.to_owned(),
            ),
            (200, "application/json", r#"{"access_token":"aad","expires_in":3600}"#.to_owned()),
        ])
        .await;
        let credential = credential(&base, &file);

        let token = credential.token("https://storage.azure.com/.default").await.unwrap();
        std::fs::write(&file, "rotated").unwrap();
        let assertion = credential.assertion().await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert_eq!(assertion, "k8s-token\n");
        assert!(requests[1].contains("client_assertion=k8s-token%0A"));
        assert!(requests[1].contains("grant_type=client_credentials"));
    }

    #[tokio::test]
    async fn an_unreadable_token_file_stops_the_chain_with_the_os_error() {
        let (base, _handle) = serve(vec![(
            200,
            "application/json",
            r#"{"authorization_endpoint":"{BASE}/t/authorize","token_endpoint":"{BASE}/t/oauth2/v2.0/token","issuer":"{BASE}/t/v2.0"}"#.to_owned(),
        )])
        .await;

        let error = credential(&base, std::path::Path::new("/nonexistent/token"))
            .token("s")
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SourceError::Failed(
                "WorkloadIdentityCredential: open /nonexistent/token: No such file or directory (os error 2)"
                    .to_owned()
            )
        );
    }
}
