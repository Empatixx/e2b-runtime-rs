use reqwest::Client;
use tokio::sync::Mutex;

use super::aad::{Authority, Exchange};
use super::msal::{CacheKey, TokenCache};
use super::tenant::TenantPolicy;
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource};

const CREDENTIAL: &str = "ClientSecretCredential";

pub struct ClientSecretCredential {
    pub client_id: String,
    pub secret: String,
    pub authority: Authority,
    pub tenants: TenantPolicy,
    pub client: Client,
    pub cache: Mutex<TokenCache>,
}

impl ClientSecretCredential {
    async fn acquire(&self, authority: &Authority, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let endpoints = authority
            .resolve(&self.client)
            .await
            .map_err(|e| e.failed(CREDENTIAL, |message| message))?;
        endpoints
            .exchange(
                &self.client,
                Exchange {
                    credential: CREDENTIAL,
                    scope: request.scope,
                    cae: request.cae,
                    form: vec![
                        ("grant_type", "client_credentials".to_owned()),
                        ("client_secret", self.secret.clone()),
                        ("client_id", self.client_id.clone()),
                    ],
                },
            )
            .await
            .map(|grant| grant.token)
    }
}

#[async_trait::async_trait]
impl TokenSource for ClientSecretCredential {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        if request.scope.is_empty() {
            return Err(SourceError::Failed(format!(
                "{CREDENTIAL}.GetToken() requires at least one scope"
            )));
        }
        let authority = self.authority.for_request(&self.tenants, CREDENTIAL, request.tenant)?;
        let key = CacheKey::of(request, authority.tenant());
        let mut cache = self.cache.lock().await;
        super::msal::confidential_token(&mut cache, key, || self.acquire(&authority, request)).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::serve;
    use super::*;

    fn credential(base: &str) -> ClientSecretCredential {
        ClientSecretCredential {
            client_id: "client".to_owned(),
            secret: "s3cret".to_owned(),
            authority: Authority::local(base),
            tenants: TenantPolicy::new("t", &[]),
            client: Client::new(),
            cache: Mutex::default(),
        }
    }

    fn configuration() -> (u16, &'static str, String) {
        (
            200,
            "application/json",
            r#"{"authorization_endpoint":"{BASE}/t/authorize","token_endpoint":"{BASE}/t/oauth2/v2.0/token","issuer":"{BASE}/t/v2.0"}"#.to_owned(),
        )
    }

    #[tokio::test]
    async fn the_secret_is_exchanged_at_the_discovered_endpoint_and_the_token_is_cached() {
        let (base, handle) = serve(vec![
            configuration(),
            (
                200,
                "application/json",
                r#"{"access_token":"aad","expires_in":3600}"#.to_owned(),
            ),
        ])
        .await;
        let credential = credential(&base);

        let first = credential
            .token_for(&TokenRequest::new("https://storage.azure.com/.default").with_cae())
            .await
            .unwrap();
        let second = credential
            .token_for(&TokenRequest::new("https://storage.azure.com/.default").with_cae())
            .await
            .unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(first.secret, "aad");
        assert_eq!(second.secret, "aad");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /t/v2.0/.well-known/openid-configuration HTTP/1.1"));
        assert!(requests[1].starts_with("POST /t/oauth2/v2.0/token HTTP/1.1"));
        assert!(requests[1].contains("client_secret=s3cret"));
        assert!(requests[1].contains("grant_type=client_credentials"));
        assert!(requests[1].contains("claims=%7B%22access_token%22"));
        assert!(!requests[1].contains("client_info"));
    }

    #[tokio::test]
    async fn rejected_secrets_read_like_the_sdk() {
        let (base, _handle) = serve(vec![
            configuration(),
            (401, "application/json", r#"{"error":"invalid_client"}"#.to_owned()),
        ])
        .await;

        let error = credential(&base).token("s").await.unwrap_err();

        let SourceError::Failed(text) = error else {
            panic!("expected a failure");
        };
        assert!(text.starts_with("ClientSecretCredential authentication failed. \nPOST http://"));
        assert!(text.ends_with("To troubleshoot, visit https://aka.ms/azsdk/go/identity/troubleshoot#client-secret"));
    }

    #[tokio::test]
    async fn other_tenants_are_refused_without_an_allow_list() {
        let credential = credential("http://127.0.0.1:1");

        let error = credential
            .token_for(&TokenRequest::new("s").with_tenant("other"))
            .await
            .unwrap_err();

        assert!(
            error
                .message()
                .starts_with("ClientSecretCredential isn't configured to acquire tokens for tenant \"other\"")
        );
    }
}
