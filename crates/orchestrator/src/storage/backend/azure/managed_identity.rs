mod arc;
mod plan;
mod source;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use reqwest::{Client, Response, StatusCode};

use super::aad::parse_managed_identity;
use super::azcore::{self, AuthenticationFailure, HttpFailure};
use super::retry::{self, RetryPolicy};
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource};
use plan::Planned;
pub use source::Identity;
use source::{Environment, Source};

const CREDENTIAL: &str = "ManagedIdentityCredential";
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const VALIDITY_MARGIN: Duration = Duration::from_secs(5 * 60);
const DEFAULT_SUFFIX: &str = "/.default";
const PROBE_TIMED_OUT: &str =
    "managed identity timed out. See https://aka.ms/azsdk/go/identity/troubleshoot#dac for more information";

pub struct ManagedIdentitySettings<'a> {
    pub identity: Identity,
    pub chained: bool,
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

pub struct ManagedIdentitySource {
    source: Source,
    identity: Identity,
    environment: Environment,
    chained: bool,
    probe: AtomicBool,
    key_directory: Option<String>,
    http: Client,
    retry: RetryPolicy,
    cache: std::sync::Mutex<HashMap<String, BearerToken>>,
    refreshing: AtomicBool,
}

enum Failure {
    Http(HttpFailure),
    InvalidJson(String),
    Other(String),
}

impl ManagedIdentitySource {
    pub fn new(settings: ManagedIdentitySettings) -> Result<Self, String> {
        let environment = Environment::read(settings.env);
        Self::with_environment(environment, settings.identity, settings.chained, std::env::consts::OS)
    }

    fn with_environment(
        environment: Environment,
        identity: Identity,
        chained: bool,
        platform: &str,
    ) -> Result<Self, String> {
        let source = environment.source();
        identity.validate_for(source)?;
        Ok(Self {
            source,
            identity,
            chained,
            probe: AtomicBool::new(source == Source::Imds && chained),
            key_directory: arc::token_directory(platform).map(str::to_owned),
            http: azcore::pipeline_client(),
            retry: match source {
                Source::Imds => RetryPolicy::imds(),
                _ => RetryPolicy::standard(),
            },
            cache: std::sync::Mutex::default(),
            refreshing: AtomicBool::new(false),
            environment,
        })
    }

    fn is_imds(&self) -> bool {
        self.source == Source::Imds
    }

    async fn probe_imds(&self) -> Result<(), SourceError> {
        if !self.probe.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.http
            .get(&self.environment.imds_url)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| SourceError::Unavailable(format!("{CREDENTIAL}: {}", self.probe_failure(&e))))?;
        self.probe.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn probe_failure(&self, error: &reqwest::Error) -> String {
        match error.is_timeout() {
            true => PROBE_TIMED_OUT.to_owned(),
            false => transport_text("GET", &self.environment.imds_url, error),
        }
    }

    async fn acquire(&self, resource: &str) -> Result<BearerToken, Failure> {
        let Some(cached) = self.cached(resource) else {
            return self.fetch(resource).await;
        };
        if !cached.refresh_due() {
            return Ok(cached);
        }
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(cached);
        }
        let renewed = self.fetch(resource).await;
        self.refreshing.store(false, Ordering::SeqCst);
        Ok(renewed.unwrap_or(cached))
    }

    fn cached(&self, resource: &str) -> Option<BearerToken> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(resource)
            .filter(|token| token.expires_at >= SystemTime::now() + VALIDITY_MARGIN)
            .cloned()
    }

    async fn fetch(&self, resource: &str) -> Result<BearerToken, Failure> {
        let body = match self.source {
            Source::AzureArc => self.challenged(resource).await?,
            source => {
                let planned = plan::plan(source, &self.identity, &self.environment, resource);
                self.body_of(self.deliver(&planned).await?, &planned).await?
            }
        };
        let token = parse_managed_identity(&body).map_err(Failure::InvalidJson)?;
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(resource.to_owned(), token.clone());
        Ok(token)
    }

    async fn challenged(&self, resource: &str) -> Result<String, Failure> {
        let first = plan::arc(&self.environment, resource, None);
        let response = self.deliver(&first).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Err(Failure::Other(format!(
                "expected a 401 response, received {}",
                response.status().as_u16()
            )));
        }
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let key = arc::secret_key(&challenge, self.key_directory.as_deref()).map_err(Failure::Other)?;
        let second = plan::arc(&self.environment, resource, Some(&key));
        self.body_of(self.deliver(&second).await?, &second).await
    }

    async fn deliver(&self, planned: &Planned) -> Result<Response, Failure> {
        let mut request = self.http.request(planned.method.clone(), &planned.url);
        for (name, value) in &planned.headers {
            request = request.header(*name, value);
        }
        if let Some(body) = &planned.body {
            request = request.body(body.clone());
        }
        retry::send(&self.retry, request)
            .await
            .map_err(|e| Failure::Other(transport_text(planned.method.as_str(), &planned.url, &e)))
    }

    async fn body_of(&self, response: Response, planned: &Planned) -> Result<String, Failure> {
        let status = response.status();
        let body = response.text().await.map_err(|e| Failure::Other(e.to_string()))?;
        match status.as_u16() {
            200 | 202 => Ok(body),
            _ => Err(Failure::Http(HttpFailure {
                method: planned.method.as_str().to_owned(),
                url: planned.url.clone(),
                status,
                body,
            })),
        }
    }

    fn classify(&self, failure: Failure) -> SourceError {
        match failure {
            Failure::InvalidJson(text) if self.is_imds() && self.chained => {
                SourceError::Unavailable(format!("{CREDENTIAL}: {text}"))
            }
            Failure::InvalidJson(text) | Failure::Other(text) => SourceError::Failed(format!("{CREDENTIAL}: {text}")),
            Failure::Http(failure) => self.http_failure(&failure),
        }
    }

    fn http_failure(&self, failure: &HttpFailure) -> SourceError {
        let body = failure.body.as_str();
        match (self.is_imds(), failure.status.as_u16()) {
            (true, 400) if self.identity.is_user_assigned() => {
                self.rendered("the requested identity isn't assigned to this resource", failure)
            }
            (true, 400) => SourceError::Unavailable(format!("{CREDENTIAL}: {}", system_assigned_failure(body))),
            (true, 403) if body.contains("unreachable") => {
                SourceError::Unavailable(format!("{CREDENTIAL}: unexpected response {body:?}"))
            }
            _ => self.rendered("", failure),
        }
    }

    fn rendered(&self, message: &str, failure: &HttpFailure) -> SourceError {
        SourceError::Failed(
            AuthenticationFailure {
                credential: CREDENTIAL,
                message,
                method: &failure.method,
                url: &failure.url,
                status: failure.status,
                body: &failure.body,
            }
            .text(),
        )
    }
}

fn transport_text(method: &str, url: &str, error: &reqwest::Error) -> String {
    let operation = match method {
        "GET" => "Get",
        "POST" => "Post",
        other => other,
    };
    format!("{operation} {url:?}: {error}")
}

fn system_assigned_failure(body: &str) -> String {
    match body.is_empty() {
        true => "failed to authenticate a system assigned identity".to_owned(),
        false => format!("failed to authenticate a system assigned identity. The endpoint responded with {body}"),
    }
}

#[async_trait::async_trait]
impl TokenSource for ManagedIdentitySource {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        self.probe_imds().await?;
        let resource = request.scope.strip_suffix(DEFAULT_SUFFIX).unwrap_or(request.scope);
        self.acquire(resource).await.map_err(|failure| self.classify(failure))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{serve, serve_raw};
    use super::*;

    const TOKEN: &str =
        r#"{"access_token":"mi-token","expires_on":"4000000000","resource":"https://storage.azure.com"}"#;

    const PROBE: (u16, &str, String) = (400, "text/plain", String::new());

    fn environment(pairs: &[(&str, &str)]) -> Environment {
        Environment {
            himds_present: false,
            ..Environment::read(&|name| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            })
        }
    }

    fn source(environment: Environment, identity: Identity, chained: bool) -> ManagedIdentitySource {
        let mut source = ManagedIdentitySource::with_environment(environment, identity, chained, "linux").unwrap();
        source.retry = source.retry.clone().without_delay();
        source
    }

    fn imds(base: &str, identity: Identity, chained: bool) -> ManagedIdentitySource {
        let mut environment = environment(&[]);
        environment.imds_url = format!("{base}/token");
        source(environment, identity, chained)
    }

    #[tokio::test]
    async fn app_service_tokens_are_requested_with_the_identity_header_and_cached() {
        let (base, handle) = serve(vec![(200, "application/json", TOKEN.to_owned())]).await;
        let source = source(
            environment(&[
                ("IDENTITY_ENDPOINT", &format!("{base}/msi")),
                ("IDENTITY_HEADER", "hdr"),
            ]),
            Identity::SystemAssigned,
            true,
        );

        let first = source
            .token_for(&TokenRequest::new("https://storage.azure.com/.default"))
            .await
            .unwrap();
        let second = source.token("https://storage.azure.com/.default").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(first.secret, "mi-token");
        assert_eq!(second.secret, "mi-token");
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .starts_with("GET /msi?api-version=2019-08-01&resource=https%3A%2F%2Fstorage.azure.com HTTP/1.1")
        );
        assert!(requests[0].contains("x-identity-header: hdr"));
    }

    #[tokio::test]
    async fn cloud_shell_posts_a_form() {
        let (base, handle) = serve(vec![(200, "application/json", TOKEN.to_owned())]).await;
        let source = source(
            environment(&[("MSI_ENDPOINT", &format!("{base}/shell"))]),
            Identity::SystemAssigned,
            true,
        );

        source.token("https://storage.azure.com/.default").await.unwrap();
        let requests = handle.await.unwrap();

        assert!(requests[0].starts_with("POST /shell HTTP/1.1"));
        assert!(requests[0].contains("metadata: true"));
        assert!(requests[0].ends_with("resource=https%3A%2F%2Fstorage.azure.com"));
    }

    #[tokio::test]
    async fn azure_ml_and_service_fabric_send_their_secrets() {
        let (ml_base, ml) = serve(vec![(200, "application/json", TOKEN.to_owned())]).await;
        let (fabric_base, fabric) = serve(vec![(200, "application/json", TOKEN.to_owned())]).await;
        let ml_source = source(
            environment(&[("MSI_ENDPOINT", &format!("{ml_base}/ml")), ("MSI_SECRET", "ml-secret")]),
            Identity::ClientId("mine".to_owned()),
            true,
        );
        let fabric_source = source(
            environment(&[
                ("IDENTITY_ENDPOINT", &format!("{fabric_base}/fabric")),
                ("IDENTITY_HEADER", "fabric-secret"),
                ("IDENTITY_SERVER_THUMBPRINT", "thumb"),
            ]),
            Identity::SystemAssigned,
            true,
        );

        ml_source.token("r/.default").await.unwrap();
        fabric_source.token("r/.default").await.unwrap();
        let ml_request = ml.await.unwrap().remove(0);
        let fabric_request = fabric.await.unwrap().remove(0);

        assert!(ml_request.contains("secret: ml-secret"));
        assert!(ml_request.starts_with("GET /ml?api-version=2017-09-01&clientid=mine&resource=r HTTP/1.1"));
        assert!(fabric_request.contains("secret: fabric-secret"));
        assert!(fabric_request.contains("accept: application/json"));
        assert!(fabric_request.starts_with("GET /fabric?api-version=2019-07-01-preview&resource=r HTTP/1.1"));
    }

    #[tokio::test]
    async fn azure_arc_answers_the_challenge_with_the_key_file() {
        let directory = tempfile::tempdir().unwrap();
        let key = directory.path().join("agent.key");
        std::fs::write(&key, "arc-secret").unwrap();
        let challenge = format!(
            "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Basic realm={}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            key.display()
        );
        let token = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{TOKEN}",
            TOKEN.len()
        );
        let (base, handle) = serve_raw(vec![challenge, token]).await;
        let mut source = source(
            environment(&[("IDENTITY_ENDPOINT", &format!("{base}/arc")), ("IMDS_ENDPOINT", "x")]),
            Identity::SystemAssigned,
            true,
        );
        source.key_directory = directory.path().to_str().map(str::to_owned);

        let granted = source.token("r/.default").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(granted.secret, "mi-token");
        assert!(!requests[0].contains("authorization:"));
        assert!(requests[1].contains("authorization: Basic arc-secret"));
        assert!(requests[1].starts_with("GET /arc?api-version=2020-06-01&resource=r HTTP/1.1"));
    }

    #[tokio::test]
    async fn azure_arc_expects_a_challenge() {
        let (base, _handle) = serve(vec![(200, "application/json", TOKEN.to_owned())]).await;
        let source = source(
            environment(&[("IDENTITY_ENDPOINT", &format!("{base}/arc")), ("IMDS_ENDPOINT", "x")]),
            Identity::SystemAssigned,
            true,
        );

        let error = source.token("r").await.unwrap_err();

        assert_eq!(
            error,
            SourceError::Failed("ManagedIdentityCredential: expected a 401 response, received 200".to_owned())
        );
    }

    #[tokio::test]
    async fn listed_imds_statuses_are_retried_six_times() {
        let replies = (0..7).map(|_| (404, "text/plain", "gone".to_owned())).collect();
        let (base, handle) = serve(replies).await;

        let error = imds(&base, Identity::SystemAssigned, false)
            .token("r/.default")
            .await
            .unwrap_err();

        assert_eq!(handle.await.unwrap().len(), 7);
        assert!(matches!(error, SourceError::Failed(text) if text.contains("RESPONSE 404: 404 Not Found")));
    }

    #[tokio::test]
    async fn a_system_assigned_identity_that_imds_rejects_lets_the_chain_move_on() {
        let (base, _handle) = serve(vec![PROBE, (400, "text/plain", "nope".to_owned())]).await;

        let error = imds(&base, Identity::SystemAssigned, true)
            .token("r")
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SourceError::Unavailable(
                "ManagedIdentityCredential: failed to authenticate a system assigned identity. The endpoint responded with nope".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn a_user_assigned_identity_that_is_not_assigned_stops_the_chain() {
        let (base, _handle) = serve(vec![
            PROBE,
            (400, "application/json", r#"{"error":"invalid_request"}"#.to_owned()),
        ])
        .await;

        let error = imds(&base, Identity::ClientId("c".to_owned()), true)
            .token("r")
            .await
            .unwrap_err();

        let SourceError::Failed(text) = error else {
            panic!("expected a failure");
        };
        assert!(text.starts_with("ManagedIdentityCredential authentication failed. the requested identity isn't assigned to this resource\nGET http://127.0.0.1:"));
        assert!(text.contains("RESPONSE 400: 400 Bad Request"));
        assert!(text.ends_with("To troubleshoot, visit https://aka.ms/azsdk/go/identity/troubleshoot#managed-id"));
    }

    #[tokio::test]
    async fn a_proxy_answering_forbidden_unreachable_lets_the_chain_move_on() {
        let (base, _handle) = serve(vec![PROBE, (403, "text/plain", "host unreachable".to_owned())]).await;

        let error = imds(&base, Identity::SystemAssigned, true)
            .token("r")
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SourceError::Unavailable("ManagedIdentityCredential: unexpected response \"host unreachable\"".to_owned())
        );
    }

    #[tokio::test]
    async fn unparsable_imds_replies_move_on_only_inside_a_chain() {
        let (chained_base, _chained) = serve(vec![PROBE, (200, "text/plain", "<html/>".to_owned())]).await;
        let (alone_base, _alone) = serve(vec![(200, "text/plain", "<html/>".to_owned())]).await;

        let chained = imds(&chained_base, Identity::SystemAssigned, true)
            .token("r")
            .await
            .unwrap_err();
        let alone = imds(&alone_base, Identity::SystemAssigned, false)
            .token("r")
            .await
            .unwrap_err();

        assert!(
            matches!(&chained, SourceError::Unavailable(text) if text.starts_with("ManagedIdentityCredential: error parsing the json error: "))
        );
        assert!(
            matches!(&alone, SourceError::Failed(text) if text.starts_with("ManagedIdentityCredential: error parsing the json error: "))
        );
    }

    #[tokio::test]
    async fn the_first_chained_imds_request_is_preceded_by_a_probe_that_any_reply_satisfies() {
        let (base, handle) = serve(vec![
            (400, "text/plain", String::new()),
            (200, "application/json", TOKEN.to_owned()),
        ])
        .await;

        let token = imds(&base, Identity::SystemAssigned, true).token("r").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "mi-token");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /token HTTP/1.1"));
        assert!(!requests[0].contains("metadata:"));
        assert!(requests[1].contains("metadata: true"));
    }

    #[tokio::test]
    async fn an_unreachable_imds_endpoint_is_unavailable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let error = imds(&base, Identity::SystemAssigned, true)
            .token("r")
            .await
            .unwrap_err();

        assert!(
            matches!(&error, SourceError::Unavailable(text) if text.starts_with("ManagedIdentityCredential: Get \"http://127.0.0.1:"))
        );
    }

    #[tokio::test]
    async fn a_token_past_its_refresh_instant_is_renewed_and_a_failed_renewal_keeps_it() {
        let (base, _handle) = serve(vec![(200, "application/json", TOKEN.replace("mi-token", "renewed"))]).await;
        let source = imds(&base, Identity::SystemAssigned, false);
        let stale = BearerToken {
            secret: "stale".to_owned(),
            expires_at: SystemTime::now() + Duration::from_secs(7200),
            refresh_on: Some(SystemTime::now() - Duration::from_secs(1)),
        };
        source.cache.lock().unwrap().insert("r".to_owned(), stale);

        let renewed = source.token("r").await.unwrap();
        source.cache.lock().unwrap().insert(
            "r".to_owned(),
            BearerToken {
                refresh_on: Some(SystemTime::now() - Duration::from_secs(1)),
                ..renewed.clone()
            },
        );
        let kept = source.token("r").await.unwrap();

        assert_eq!(renewed.secret, "renewed");
        assert_eq!(kept.secret, "renewed");
    }

    #[test]
    fn user_assigned_identities_are_validated_when_the_source_is_chosen() {
        let error = ManagedIdentitySource::with_environment(
            environment(&[("MSI_ENDPOINT", "x")]),
            Identity::ClientId("c".to_owned()),
            true,
            "linux",
        )
        .err()
        .unwrap();

        assert_eq!(error, "Cloud Shell doesn't support user-assigned managed identities");
    }
}
