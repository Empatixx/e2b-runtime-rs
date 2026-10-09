use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

use super::ecr::BasicCredentials;
use crate::storage::backend::azure::azcore::{self, ResponseError};
use crate::storage::backend::azure::default_credential::DefaultAzureCredential;

const TOKEN_USERNAME: &str = "00000000-0000-0000-0000-000000000000";
const AAD_SCOPE: &str = "https://containerregistry.azure.net/.default";
const API_VERSION: &str = "2021-07-01";
const WALK_TIMEOUT: Duration = Duration::from_secs(45);
const SOFT_TTL: Duration = Duration::from_secs(60 * 60);
const HARD_TTL: Duration = Duration::from_secs(150 * 60);

#[derive(Clone)]
pub struct AcrAuthenticator {
    inner: Arc<Inner>,
}

struct Inner {
    login_server: String,
    base_url: String,
    credential: Arc<DefaultAzureCredential>,
    client: Client,
    cache: std::sync::Mutex<Cache>,
    walk: AsyncMutex<()>,
}

#[derive(Default)]
struct Cache {
    token: Option<String>,
    soft_expiry: Option<Instant>,
    hard_expiry: Option<Instant>,
}

#[derive(Deserialize)]
struct Exchange {
    #[serde(default)]
    refresh_token: Option<String>,
}

impl Cache {
    fn token_before(&self, expiry: Option<Instant>) -> Option<String> {
        self.token
            .clone()
            .filter(|_| expiry.is_some_and(|expiry| Instant::now() < expiry))
    }

    fn fresh(&self) -> Option<String> {
        self.token_before(self.soft_expiry)
    }

    fn stale(&self) -> Option<String> {
        self.token_before(self.hard_expiry)
    }

    fn store(&mut self, token: String) {
        let now = Instant::now();
        *self = Self {
            token: Some(token),
            soft_expiry: Some(now + SOFT_TTL),
            hard_expiry: Some(now + HARD_TTL),
        };
    }
}

impl std::fmt::Debug for AcrAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcrAuthenticator")
            .field("login_server", &self.inner.login_server)
            .finish_non_exhaustive()
    }
}

impl PartialEq for AcrAuthenticator {
    fn eq(&self, other: &Self) -> bool {
        self.inner.login_server == other.inner.login_server
    }
}

impl Eq for AcrAuthenticator {}

impl AcrAuthenticator {
    pub fn new(login_server: &str, credential: Arc<DefaultAzureCredential>) -> Self {
        Self::with_base_url(login_server, format!("https://{login_server}"), credential)
    }

    pub fn with_base_url(login_server: &str, base_url: String, credential: Arc<DefaultAzureCredential>) -> Self {
        Self {
            inner: Arc::new(Inner {
                login_server: login_server.to_owned(),
                base_url,
                credential,
                client: azcore::pipeline_client(),
                cache: std::sync::Mutex::default(),
                walk: AsyncMutex::new(()),
            }),
        }
    }

    pub async fn authorization(&self) -> Result<BasicCredentials, String> {
        let token = match self.inner.cached() {
            Some(token) => token,
            None => {
                let inner = self.inner.clone();
                tokio::spawn(async move { inner.lead().await })
                    .await
                    .map_err(|e| e.to_string())??
            }
        };
        Ok(BasicCredentials {
            username: TOKEN_USERNAME.to_owned(),
            password: token,
        })
    }

    pub fn invalidate(&self) {
        *self.inner.cache() = Cache::default();
    }
}

impl Inner {
    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn cached(&self) -> Option<String> {
        self.cache().fresh()
    }

    async fn lead(&self) -> Result<String, String> {
        let _walk = self.walk.lock().await;
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let walked = tokio::time::timeout(WALK_TIMEOUT, self.walk())
            .await
            .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()));
        match walked {
            Ok(token) => {
                self.cache().store(token.clone());
                Ok(token)
            }
            Err(e) => self.cache().stale().ok_or(e),
        }
    }

    async fn walk(&self) -> Result<String, String> {
        let access = self
            .credential
            .token(AAD_SCOPE)
            .await
            .map_err(|e| format!("failed to get AAD access token: {e}"))?;
        let refresh = self
            .exchange(&access.secret)
            .await
            .map_err(|e| format!("failed to exchange AAD token for ACR refresh token: {e}"))?;
        refresh
            .filter(|token| !token.is_empty())
            .ok_or_else(|| "acr token exchange response did not contain a refresh token".to_owned())
    }

    async fn exchange(&self, access_token: &str) -> Result<Option<String>, String> {
        let url = format!("{}/oauth2/exchange?api-version={API_VERSION}", self.base_url);
        let request = self.client.post(&url).header("Accept", "application/json").form(&[
            ("access_token", access_token),
            ("grant_type", "access_token"),
            ("service", self.login_server.as_str()),
        ]);
        let response = azcore::send(request).await?;
        let status = response.status();
        let error_code = response
            .headers()
            .get("x-ms-error-code")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await.map_err(|e| e.to_string())?;
        match status {
            StatusCode::OK => serde_json::from_str::<Exchange>(&body)
                .map(|exchange| exchange.refresh_token)
                .map_err(|e| e.to_string()),
            _ => Err(ResponseError {
                method: "POST",
                url: &url,
                status,
                error_code,
                body: &body,
            }
            .text()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use super::*;
    use crate::storage::backend::azure::testing::read_request;

    struct Registry {
        requests: Arc<Mutex<Vec<String>>>,
        authenticator: AcrAuthenticator,
    }

    async fn registry(answer: impl Fn(usize) -> (u16, &'static str) + Send + Sync + 'static) -> Registry {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let count = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let (mut connection, _) = listener.accept().await.unwrap();
                let request = read_request(&mut connection).await;
                log.lock().unwrap().push(request);
                let (status, body) = answer(count.fetch_add(1, Ordering::SeqCst));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                connection.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        let authenticator = AcrAuthenticator::with_base_url(
            "reg.azurecr.io",
            format!("http://{address}"),
            Arc::new(DefaultAzureCredential::with_static_token("aad-access-token")),
        );
        Registry {
            requests,
            authenticator,
        }
    }

    const REFRESH: &str = r#"{"refresh_token":"acr-refresh-token"}"#;

    #[tokio::test]
    async fn the_aad_token_is_exchanged_for_a_refresh_token() {
        let registry = registry(|_| (200, REFRESH)).await;

        let credentials = registry.authenticator.authorization().await.unwrap();

        assert_eq!(credentials.username, "00000000-0000-0000-0000-000000000000");
        assert_eq!(credentials.password, "acr-refresh-token");
        let sent = registry.requests.lock().unwrap()[0].clone();
        assert!(sent.starts_with("POST /oauth2/exchange?api-version=2021-07-01 HTTP/1.1"));
        assert!(sent.contains("content-type: application/x-www-form-urlencoded"));
        assert!(sent.ends_with("access_token=aad-access-token&grant_type=access_token&service=reg.azurecr.io"));
    }

    #[tokio::test]
    async fn the_refresh_token_is_cached_until_invalidated() {
        let registry = registry(|_| (200, REFRESH)).await;

        for _ in 0..3 {
            registry.authenticator.authorization().await.unwrap();
        }
        assert_eq!(registry.requests.lock().unwrap().len(), 1);

        registry.authenticator.invalidate();
        registry.authenticator.authorization().await.unwrap();
        assert_eq!(registry.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_denied_exchange_surfaces_the_status() {
        let registry = registry(|_| (403, r#"{"errors":[{"code":"DENIED"}]}"#)).await;

        let error = registry.authenticator.authorization().await.unwrap_err();

        assert!(error.starts_with("failed to exchange AAD token for ACR refresh token: POST http://"));
        assert!(error.contains("RESPONSE 403: 403 Forbidden"));
    }

    #[tokio::test]
    async fn an_empty_refresh_token_is_rejected() {
        let registry = registry(|_| (200, "{}")).await;

        assert_eq!(
            registry.authenticator.authorization().await.unwrap_err(),
            "acr token exchange response did not contain a refresh token"
        );
    }

    #[tokio::test]
    async fn a_soft_expired_token_is_served_while_the_exchange_fails() {
        let registry = registry(|index| match index {
            0 => (200, REFRESH),
            _ => (403, "{}"),
        })
        .await;
        registry.authenticator.authorization().await.unwrap();
        registry.authenticator.inner.cache().soft_expiry = Some(Instant::now() - Duration::from_secs(60));

        let credentials = registry.authenticator.authorization().await.unwrap();

        assert_eq!(credentials.password, "acr-refresh-token");
        assert_eq!(registry.requests.lock().unwrap().len(), 2);

        registry.authenticator.inner.cache().hard_expiry = Some(Instant::now() - Duration::from_secs(60));
        assert!(registry.authenticator.authorization().await.is_err());
    }
}
