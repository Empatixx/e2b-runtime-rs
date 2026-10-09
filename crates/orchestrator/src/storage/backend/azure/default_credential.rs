use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use super::aad::{Authority, ClientKind};
use super::azcore;
use super::azure_cli::AzureCliCredential;
use super::azure_developer_cli::AzureDeveloperCliCredential;
use super::client_certificate::{ClientCertificate, ClientCertificateCredential};
use super::client_secret::ClientSecretCredential;
use super::managed_identity::{Identity, ManagedIdentitySettings, ManagedIdentitySource};
use super::msal::CacheKey;
use super::powershell::PowerShellCredential;
use super::tenant::TenantPolicy;
pub use super::token_source::BearerToken;
use super::token_source::{SourceError, TokenRequest, TokenSource};
use super::username_password::UsernamePasswordCredential;
use super::workload_identity::WorkloadIdentityCredential;

const CHAIN_NAME: &str = "DefaultAzureCredential";
const TOKEN_CREDENTIALS_ENV: &str = "AZURE_TOKEN_CREDENTIALS";
const TENANT_ENV: &str = "AZURE_TENANT_ID";
const CLIENT_ENV: &str = "AZURE_CLIENT_ID";
const SECRET_ENV: &str = "AZURE_CLIENT_SECRET";
const CERTIFICATE_ENV: &str = "AZURE_CLIENT_CERTIFICATE_PATH";
const CERTIFICATE_PASSWORD_ENV: &str = "AZURE_CLIENT_CERTIFICATE_PASSWORD";
const SEND_CHAIN_ENV: &str = "AZURE_CLIENT_SEND_CERTIFICATE_CHAIN";
const USERNAME_ENV: &str = "AZURE_USERNAME";
const PASSWORD_ENV: &str = "AZURE_PASSWORD";
const EAGER_BACKOFF: Duration = Duration::from_secs(30);

struct Source {
    name: &'static str,
    credential: Result<Arc<dyn TokenSource>, String>,
}

pub struct DefaultAzureCredential {
    sources: Vec<Source>,
    selected: Mutex<Option<usize>>,
    resources: std::sync::Mutex<HashMap<CacheKey, Resource>>,
    renewing: Mutex<()>,
}

#[derive(Clone)]
struct Resource {
    token: BearerToken,
    attempted_at: SystemTime,
}

impl Resource {
    fn attempted_now(token: BearerToken) -> Self {
        Self {
            token,
            attempted_at: SystemTime::now(),
        }
    }

    fn wants_refresh(&self) -> bool {
        let now = SystemTime::now();
        self.token.refresh_instant() < now && self.attempted_at + EAGER_BACKOFF <= now
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Environment,
    WorkloadIdentity,
    ManagedIdentity,
    AzureCli,
    AzureDeveloperCli,
    AzurePowerShell,
}

const ALL: [Kind; 6] = [
    Kind::Environment,
    Kind::WorkloadIdentity,
    Kind::ManagedIdentity,
    Kind::AzureCli,
    Kind::AzureDeveloperCli,
    Kind::AzurePowerShell,
];
const PROD: [Kind; 3] = [Kind::Environment, Kind::WorkloadIdentity, Kind::ManagedIdentity];
const DEV: [Kind; 3] = [Kind::AzureCli, Kind::AzureDeveloperCli, Kind::AzurePowerShell];

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Environment => "EnvironmentCredential",
            Kind::WorkloadIdentity => "WorkloadIdentityCredential",
            Kind::ManagedIdentity => "ManagedIdentityCredential",
            Kind::AzureCli => "AzureCLICredential",
            Kind::AzureDeveloperCli => "AzureDeveloperCLICredential",
            Kind::AzurePowerShell => "AzurePowerShellCredential",
        }
    }

    fn build(self, chained: bool) -> Result<Arc<dyn TokenSource>, String> {
        match self {
            Kind::Environment => environment_credential(),
            Kind::WorkloadIdentity => WorkloadIdentityCredential::from_environment(&raw_variable)
                .map(|credential| Arc::new(credential) as Arc<dyn TokenSource>),
            Kind::ManagedIdentity => managed_identity_credential(chained),
            Kind::AzureCli => Ok(Arc::new(AzureCliCredential::new(developer_tenants()))),
            Kind::AzureDeveloperCli => Ok(Arc::new(AzureDeveloperCliCredential::new(developer_tenants()))),
            Kind::AzurePowerShell => Ok(Arc::new(PowerShellCredential::new(developer_tenants()))),
        }
    }
}

fn selected_kinds(setting: Option<&str>) -> Result<Vec<Kind>, String> {
    match setting {
        None => Ok(ALL.to_vec()),
        Some("dev") => Ok(DEV.to_vec()),
        Some("prod") => Ok(PROD.to_vec()),
        Some(name) => ALL
            .into_iter()
            .find(|kind| kind.name().eq_ignore_ascii_case(name))
            .map(|kind| vec![kind])
            .ok_or_else(|| {
                format!(
                    "invalid {TOKEN_CREDENTIALS_ENV} value {}. Valid values are \"dev\", \"prod\", or the name of any credential type in the default chain. See https://aka.ms/azsdk/go/identity/docs#DefaultAzureCredential for more information",
                    crate::go_strconv::quote(name)
                )
            }),
    }
}

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn raw_variable(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn developer_tenants() -> TenantPolicy {
    TenantPolicy::from_environment("", &variable)
}

fn environment_credential() -> Result<Arc<dyn TokenSource>, String> {
    environment_credential_from(&variable)
}

fn environment_credential_from(env: &dyn Fn(&str) -> Option<String>) -> Result<Arc<dyn TokenSource>, String> {
    let tenant = env(TENANT_ENV).ok_or_else(|| format!("missing environment variable {TENANT_ENV}"))?;
    let client = env(CLIENT_ENV).ok_or_else(|| format!("missing environment variable {CLIENT_ENV}"))?;
    let tenants = TenantPolicy::from_environment(&tenant, env);
    match (env(SECRET_ENV), env(CERTIFICATE_ENV), env(USERNAME_ENV)) {
        (Some(secret), _, _) => secret_credential(tenants, client, secret),
        (None, Some(path), _) => certificate_credential(env, tenants, client, &path),
        (None, None, Some(username)) => password_credential(env, tenants, client, username),
        (None, None, None) => Err(format!(
            "incomplete environment variable configuration. Only {TENANT_ENV} and {CLIENT_ENV} are set"
        )),
    }
}

fn secret_credential(tenants: TenantPolicy, client_id: String, secret: String) -> Result<Arc<dyn TokenSource>, String> {
    Ok(Arc::new(ClientSecretCredential {
        client_id,
        secret,
        authority: Authority::for_tenant(tenants.default_tenant(), ClientKind::Confidential)?,
        tenants,
        client: azcore::msal_client(),
        cache: tokio::sync::Mutex::default(),
    }))
}

fn certificate_credential(
    env: &dyn Fn(&str) -> Option<String>,
    tenants: TenantPolicy,
    client_id: String,
    path: &str,
) -> Result<Arc<dyn TokenSource>, String> {
    let password = env(CERTIFICATE_PASSWORD_ENV).unwrap_or_default();
    let send_chain = env(SEND_CHAIN_ENV).is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    Ok(Arc::new(ClientCertificateCredential {
        client_id,
        certificate: ClientCertificate::load(path, &password)?,
        send_chain,
        authority: Authority::for_tenant(tenants.default_tenant(), ClientKind::Confidential)?,
        tenants,
        client: azcore::msal_client(),
        cache: tokio::sync::Mutex::default(),
    }))
}

fn password_credential(
    env: &dyn Fn(&str) -> Option<String>,
    tenants: TenantPolicy,
    client_id: String,
    username: String,
) -> Result<Arc<dyn TokenSource>, String> {
    let password = env(PASSWORD_ENV).ok_or_else(|| format!("no value for {PASSWORD_ENV}"))?;
    Ok(Arc::new(UsernamePasswordCredential {
        client_id,
        username,
        password,
        authority: Authority::for_tenant(tenants.default_tenant(), ClientKind::Public)?,
        tenants,
        client: azcore::msal_client(),
        cache: tokio::sync::Mutex::default(),
    }))
}

fn managed_identity_credential(chained: bool) -> Result<Arc<dyn TokenSource>, String> {
    ManagedIdentitySource::new(ManagedIdentitySettings {
        identity: Identity::from_client_id_variable(std::env::var(CLIENT_ENV).ok()),
        chained,
        env: &variable,
    })
    .map(|source| Arc::new(source) as Arc<dyn TokenSource>)
}

impl std::fmt::Debug for DefaultAzureCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(CHAIN_NAME).finish_non_exhaustive()
    }
}

impl DefaultAzureCredential {
    pub fn new() -> Result<Self, String> {
        let setting = std::env::var(TOKEN_CREDENTIALS_ENV).ok();
        selected_kinds(setting.as_deref()).map(|kinds| {
            let chained = kinds.len() > 1;
            Self::from_sources(
                kinds
                    .into_iter()
                    .map(|kind| Source {
                        name: kind.name(),
                        credential: kind.build(chained),
                    })
                    .collect(),
            )
        })
    }

    #[cfg(test)]
    pub fn with_static_token(secret: &str) -> Self {
        struct Fixed(String);

        #[async_trait::async_trait]
        impl TokenSource for Fixed {
            async fn token_for(&self, _request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
                Ok(BearerToken::without_refresh(
                    self.0.clone(),
                    super::token_source::expiry(4_000_000_000),
                ))
            }
        }

        Self::from_sources(vec![Source {
            name: "Fixed",
            credential: Ok(Arc::new(Fixed(secret.to_owned()))),
        }])
    }

    #[cfg(test)]
    fn age_attempts(&self) {
        for resource in self
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
        {
            resource.attempted_at = SystemTime::now() - EAGER_BACKOFF * 2;
        }
    }

    fn from_sources(sources: Vec<Source>) -> Self {
        Self {
            sources,
            selected: Mutex::new(None),
            resources: std::sync::Mutex::default(),
            renewing: Mutex::new(()),
        }
    }

    pub async fn token(&self, scope: &str) -> Result<BearerToken, String> {
        self.token_for(&TokenRequest::new(scope)).await
    }

    pub async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, String> {
        let key = CacheKey::of(request, request.tenant);
        match self.resource(&key) {
            Some(held) if held.token.expires_at > SystemTime::now() => self.eager_refresh(request, key, held).await,
            _ => self.renew_expired(request, key).await,
        }
    }

    async fn eager_refresh(
        &self,
        request: &TokenRequest<'_>,
        key: CacheKey,
        held: Resource,
    ) -> Result<BearerToken, String> {
        if !held.wants_refresh() {
            return Ok(held.token);
        }
        let Ok(_attempt) = self.renewing.try_lock() else {
            return Ok(held.token);
        };
        self.record_attempt(&key);
        Ok(self.fetch(request, &key).await.unwrap_or(held.token))
    }

    async fn renew_expired(&self, request: &TokenRequest<'_>, key: CacheKey) -> Result<BearerToken, String> {
        let _single = self.renewing.lock().await;
        match self
            .resource(&key)
            .filter(|held| held.token.expires_at > SystemTime::now())
        {
            Some(held) => Ok(held.token),
            None => self.fetch(request, &key).await,
        }
    }

    async fn fetch(&self, request: &TokenRequest<'_>, key: &CacheKey) -> Result<BearerToken, String> {
        let token = self.acquire(request).await?;
        self.insert(key, Resource::attempted_now(token.clone()));
        Ok(token)
    }

    fn resource(&self, key: &CacheKey) -> Option<Resource> {
        self.resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn record_attempt(&self, key: &CacheKey) {
        if let Some(held) = self.resource(key) {
            self.insert(key, Resource::attempted_now(held.token));
        }
    }

    fn insert(&self, key: &CacheKey, resource: Resource) {
        self.resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), resource);
    }

    async fn acquire(&self, request: &TokenRequest<'_>) -> Result<BearerToken, String> {
        let mut selected = self.selected.lock().await;
        match *selected {
            Some(index) => {
                drop(selected);
                self.source_token(index, request)
                    .await
                    .map_err(|e| e.message().to_owned())
            }
            None => self.probe(request, &mut selected).await,
        }
    }

    async fn probe(&self, request: &TokenRequest<'_>, selected: &mut Option<usize>) -> Result<BearerToken, String> {
        let mut failures = Vec::new();
        for index in 0..self.sources.len() {
            match self.source_token(index, request).await {
                Ok(token) => {
                    *selected = Some(index);
                    return Ok(token);
                }
                Err(SourceError::Unavailable(message)) => failures.push(message),
                Err(SourceError::Failed(message)) => {
                    failures.push(message);
                    break;
                }
            }
        }
        Err(chain_error(&failures))
    }

    async fn source_token(&self, index: usize, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let source = &self.sources[index];
        match &source.credential {
            Err(e) => Err(SourceError::Unavailable(format!("{}: {e}", source.name))),
            Ok(credential) => credential.token_for(request).await,
        }
    }
}

fn chain_error(failures: &[String]) -> String {
    let attempted: String = failures
        .iter()
        .map(|failure| format!("\n\t{}", failure.replace('\n', "\n\t\t")))
        .collect();
    format!("{CHAIN_NAME}: failed to acquire a token.\nAttempted credentials:{attempted}")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::token_source::expiry;
    use super::*;

    enum Outcome {
        Token(&'static str),
        Unavailable(&'static str),
        Failed,
    }

    struct Scripted {
        outcome: Outcome,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TokenSource for Scripted {
        async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                Outcome::Token(token) => Ok(BearerToken::without_refresh(
                    format!("{token}@{}", request.scope),
                    expiry(4_000_000_000),
                )),
                Outcome::Unavailable(message) => Err(SourceError::Unavailable(message.to_owned())),
                Outcome::Failed => Err(SourceError::Failed(
                    "ClientSecretCredential authentication failed. denied".to_owned(),
                )),
            }
        }
    }

    fn scripted(name: &'static str, outcome: Outcome, calls: &Arc<AtomicUsize>) -> Source {
        Source {
            name,
            credential: Ok(Arc::new(Scripted {
                outcome,
                calls: calls.clone(),
            })),
        }
    }

    fn counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    #[tokio::test]
    async fn the_first_source_that_answers_wins_and_is_remembered() {
        let (first, second) = (counter(), counter());
        let chain = DefaultAzureCredential::from_sources(vec![
            scripted(
                "EnvironmentCredential",
                Outcome::Unavailable("EnvironmentCredential: nothing"),
                &first,
            ),
            scripted("ManagedIdentityCredential", Outcome::Token("mi"), &second),
        ]);

        let token = chain.token("https://scope/.default").await.unwrap();
        chain.token("https://other/.default").await.unwrap();

        assert_eq!(token.secret, "mi@https://scope/.default");
        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn tokens_are_cached_per_scope_until_close_to_expiry() {
        let calls = counter();
        let chain = DefaultAzureCredential::from_sources(vec![scripted("S", Outcome::Token("t"), &calls)]);

        chain.token("a").await.unwrap();
        chain.token("a").await.unwrap();
        chain.token("b").await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_authentication_failure_stops_the_chain() {
        let (failing, never) = (counter(), counter());
        let chain = DefaultAzureCredential::from_sources(vec![
            scripted("EnvironmentCredential", Outcome::Failed, &failing),
            scripted("ManagedIdentityCredential", Outcome::Token("mi"), &never),
        ]);

        let error = chain.token("s").await.unwrap_err();

        assert_eq!(
            error,
            "DefaultAzureCredential: failed to acquire a token.\nAttempted credentials:\n\tClientSecretCredential authentication failed. denied"
        );
        assert_eq!(never.load(Ordering::SeqCst), 0);
    }

    struct Flaky {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TokenSource for Flaky {
        async fn token_for(&self, _request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(BearerToken::without_refresh("first".to_owned(), expiry(4_000_000_000))),
                _ => Err(SourceError::Failed(
                    "ManagedIdentityCredential authentication failed. denied".to_owned(),
                )),
            }
        }
    }

    #[tokio::test]
    async fn a_selected_source_reports_its_own_failures_unchanged() {
        let chain = DefaultAzureCredential::from_sources(vec![Source {
            name: "ManagedIdentityCredential",
            credential: Ok(Arc::new(Flaky {
                calls: AtomicUsize::new(0),
            })),
        }]);

        chain.token("a").await.unwrap();
        let error = chain.token("b").await.unwrap_err();

        assert_eq!(error, "ManagedIdentityCredential authentication failed. denied");
    }

    #[tokio::test]
    async fn concurrent_first_calls_probe_the_chain_once() {
        let (first, second) = (counter(), counter());
        let chain = Arc::new(DefaultAzureCredential::from_sources(vec![
            scripted(
                "EnvironmentCredential",
                Outcome::Unavailable("EnvironmentCredential: nothing"),
                &first,
            ),
            scripted("ManagedIdentityCredential", Outcome::Token("mi"), &second),
        ]));

        let tasks: Vec<_> = (0..8)
            .map(|index| {
                let chain = chain.clone();
                tokio::spawn(async move { chain.token(&format!("scope-{index}")).await })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 8);
    }

    struct Sequence {
        replies: std::sync::Mutex<std::collections::VecDeque<Result<BearerToken, SourceError>>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TokenSource for Sequence {
        async fn token_for(&self, _request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(SourceError::Failed("script exhausted".to_owned())))
        }
    }

    fn sequence(replies: Vec<Result<BearerToken, SourceError>>, calls: &Arc<AtomicUsize>) -> DefaultAzureCredential {
        DefaultAzureCredential::from_sources(vec![Source {
            name: "S",
            credential: Ok(Arc::new(Sequence {
                replies: std::sync::Mutex::new(replies.into()),
                calls: calls.clone(),
            })),
        }])
    }

    fn token_at(secret: &str, expires_in: i64, refresh_in: Option<i64>) -> BearerToken {
        let at = |seconds: i64| match u64::try_from(seconds) {
            Ok(seconds) => SystemTime::now() + Duration::from_secs(seconds),
            Err(_) => SystemTime::now() - Duration::from_secs(seconds.unsigned_abs()),
        };
        BearerToken {
            secret: secret.to_owned(),
            expires_at: at(expires_in),
            refresh_on: refresh_in.map(at),
        }
    }

    #[tokio::test]
    async fn a_token_past_its_refresh_instant_is_renewed_eagerly() {
        let calls = counter();
        let chain = sequence(
            vec![Ok(token_at("old", 3600, Some(-1))), Ok(token_at("new", 3600, None))],
            &calls,
        );

        let first = chain.token("s").await.unwrap();
        chain.age_attempts();
        let second = chain.token("s").await.unwrap();

        assert_eq!((first.secret.as_str(), second.secret.as_str()), ("old", "new"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_eager_renewal_keeps_the_held_token_and_backs_off() {
        let calls = counter();
        let chain = sequence(vec![Ok(token_at("old", 120, None))], &calls);

        let first = chain.token("s").await.unwrap();
        chain.age_attempts();
        let second = chain.token("s").await.unwrap();
        let third = chain.token("s").await.unwrap();

        assert_eq!(
            (first.secret.as_str(), second.secret.as_str(), third.secret.as_str()),
            ("old", "old", "old")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_expired_token_makes_renewal_errors_visible() {
        let calls = counter();
        let chain = sequence(vec![Ok(token_at("old", -1, None))], &calls);

        chain.token("s").await.unwrap();
        let error = chain.token("s").await.unwrap_err();

        assert_eq!(error, "script exhausted");
    }

    #[tokio::test]
    async fn tenants_and_cae_get_their_own_cache_entries() {
        let calls = counter();
        let chain = DefaultAzureCredential::from_sources(vec![scripted("S", Outcome::Token("t"), &calls)]);

        chain.token_for(&TokenRequest::new("a")).await.unwrap();
        chain.token_for(&TokenRequest::new("a").with_cae()).await.unwrap();
        chain
            .token_for(&TokenRequest::new("a").with_tenant("other"))
            .await
            .unwrap();
        chain
            .token_for(&TokenRequest::new("a").with_tenant("other"))
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_chain_without_an_answer_lists_every_attempt() {
        let calls = counter();
        let chain = DefaultAzureCredential::from_sources(vec![
            Source {
                name: "EnvironmentCredential",
                credential: Err("missing environment variable AZURE_TENANT_ID".to_owned()),
            },
            scripted(
                "ManagedIdentityCredential",
                Outcome::Unavailable("ManagedIdentityCredential: nothing here"),
                &calls,
            ),
        ]);

        let error = chain.token("s").await.unwrap_err();

        assert_eq!(
            error,
            "DefaultAzureCredential: failed to acquire a token.\nAttempted credentials:\n\tEnvironmentCredential: missing environment variable AZURE_TENANT_ID\n\tManagedIdentityCredential: nothing here"
        );
    }

    #[test]
    fn the_token_credentials_setting_selects_sources() {
        assert_eq!(selected_kinds(None).unwrap().len(), 6);
        assert_eq!(selected_kinds(Some("prod")).unwrap(), PROD.to_vec());
        assert_eq!(selected_kinds(Some("dev")).unwrap(), DEV.to_vec());
        assert_eq!(
            selected_kinds(Some("managedidentitycredential")).unwrap(),
            vec![Kind::ManagedIdentity]
        );
        assert_eq!(
            selected_kinds(Some("azurecliCREDENTIAL")).unwrap(),
            vec![Kind::AzureCli]
        );
        assert_eq!(
            selected_kinds(Some("AzureDeveloperCLICredential")).unwrap(),
            vec![Kind::AzureDeveloperCli]
        );
        assert_eq!(
            selected_kinds(Some("AzurePowerShellCredential")).unwrap(),
            vec![Kind::AzurePowerShell]
        );
        assert!(
            selected_kinds(Some("nope"))
                .unwrap_err()
                .starts_with("invalid AZURE_TOKEN_CREDENTIALS value \"nope\". Valid values are")
        );
    }

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_environment_credential_selects_like_the_sdk() {
        let outcome =
            |pairs: &'static [(&'static str, &'static str)]| environment_credential_from(&env_of(pairs)).err();

        assert_eq!(outcome(&[]).unwrap(), "missing environment variable AZURE_TENANT_ID");
        assert_eq!(
            outcome(&[("AZURE_TENANT_ID", "t")]).unwrap(),
            "missing environment variable AZURE_CLIENT_ID"
        );
        assert_eq!(
            outcome(&[("AZURE_TENANT_ID", "t"), ("AZURE_CLIENT_ID", "c")]).unwrap(),
            "incomplete environment variable configuration. Only AZURE_TENANT_ID and AZURE_CLIENT_ID are set"
        );
        assert_eq!(
            outcome(&[
                ("AZURE_TENANT_ID", "t"),
                ("AZURE_CLIENT_ID", "c"),
                ("AZURE_USERNAME", "u")
            ])
            .unwrap(),
            "no value for AZURE_PASSWORD"
        );
        assert!(
            outcome(&[
                ("AZURE_TENANT_ID", "t"),
                ("AZURE_CLIENT_ID", "c"),
                ("AZURE_CLIENT_CERTIFICATE_PATH", "/nonexistent.pem")
            ])
            .unwrap()
            .starts_with("failed to read certificate file \"/nonexistent.pem\": ")
        );
        assert!(
            outcome(&[
                ("AZURE_TENANT_ID", "t"),
                ("AZURE_CLIENT_ID", "c"),
                ("AZURE_USERNAME", "u"),
                ("AZURE_PASSWORD", "p")
            ])
            .is_none()
        );
        assert!(
            outcome(&[
                ("AZURE_TENANT_ID", "t"),
                ("AZURE_CLIENT_ID", "c"),
                ("AZURE_CLIENT_SECRET", "s"),
                ("AZURE_CLIENT_CERTIFICATE_PATH", "/nonexistent.pem")
            ])
            .is_none()
        );
    }
}
