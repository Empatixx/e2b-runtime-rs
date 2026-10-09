use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use azure_core::credentials::{Secret, TokenCredential};
use azure_identity::{
    AzureCliCredential, AzureDeveloperCliCredential, ClientSecretCredential, WorkloadIdentityCredential,
};
use tokio::sync::Mutex;

use super::aad::Authority;
use super::azcore;
use super::client_certificate::{ClientCertificate, ClientCertificateCredential};
use super::managed_identity::{ManagedIdentitySettings, ManagedIdentitySource};
use super::powershell::PowerShellCredential;
pub use super::token_source::BearerToken;
use super::token_source::{SdkSource, SourceError, TokenSource};
use super::username_password::UsernamePasswordCredential;

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
const CACHE_MARGIN: Duration = Duration::from_secs(5 * 60);

struct Source {
    name: &'static str,
    credential: Result<Arc<dyn TokenSource>, String>,
}

pub struct DefaultAzureCredential {
    sources: Vec<Source>,
    selected: Mutex<Option<usize>>,
    cache: std::sync::Mutex<HashMap<String, BearerToken>>,
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
            Kind::WorkloadIdentity => WorkloadIdentityCredential::new(None)
                .map(sdk_source)
                .map_err(|e| e.to_string()),
            Kind::ManagedIdentity => managed_identity_credential(chained),
            Kind::AzureCli => AzureCliCredential::new(None).map(sdk_source).map_err(|e| e.to_string()),
            Kind::AzureDeveloperCli => AzureDeveloperCliCredential::new(None)
                .map(sdk_source)
                .map_err(|e| e.to_string()),
            Kind::AzurePowerShell => Ok(Arc::new(PowerShellCredential::default())),
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

fn sdk_source<T: TokenCredential + 'static>(credential: Arc<T>) -> Arc<dyn TokenSource> {
    Arc::new(SdkSource::new(credential))
}

fn environment_credential() -> Result<Arc<dyn TokenSource>, String> {
    environment_credential_from(&variable)
}

fn environment_credential_from(env: &dyn Fn(&str) -> Option<String>) -> Result<Arc<dyn TokenSource>, String> {
    let tenant = env(TENANT_ENV).ok_or_else(|| format!("missing environment variable {TENANT_ENV}"))?;
    let client = env(CLIENT_ENV).ok_or_else(|| format!("missing environment variable {CLIENT_ENV}"))?;
    match (env(SECRET_ENV), env(CERTIFICATE_ENV), env(USERNAME_ENV)) {
        (Some(secret), _, _) => secret_credential(&tenant, client, secret),
        (None, Some(path), _) => certificate_credential(env, &tenant, client, &path),
        (None, None, Some(username)) => password_credential(env, &tenant, client, username),
        (None, None, None) => Err(format!(
            "incomplete environment variable configuration. Only {TENANT_ENV} and {CLIENT_ENV} are set"
        )),
    }
}

fn secret_credential(tenant: &str, client_id: String, secret: String) -> Result<Arc<dyn TokenSource>, String> {
    Authority::for_tenant(tenant)?;
    ClientSecretCredential::new(tenant, client_id, Secret::new(secret), None)
        .map(sdk_source)
        .map_err(|e| e.to_string())
}

fn certificate_credential(
    env: &dyn Fn(&str) -> Option<String>,
    tenant: &str,
    client_id: String,
    path: &str,
) -> Result<Arc<dyn TokenSource>, String> {
    let password = env(CERTIFICATE_PASSWORD_ENV).unwrap_or_default();
    let send_chain = env(SEND_CHAIN_ENV).is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    Ok(Arc::new(ClientCertificateCredential {
        client_id,
        certificate: ClientCertificate::load(path, &password)?,
        send_chain,
        authority: Authority::for_tenant(tenant)?,
        client: azcore::msal_client(),
    }))
}

fn password_credential(
    env: &dyn Fn(&str) -> Option<String>,
    tenant: &str,
    client_id: String,
    username: String,
) -> Result<Arc<dyn TokenSource>, String> {
    let password = env(PASSWORD_ENV).ok_or_else(|| format!("no value for {PASSWORD_ENV}"))?;
    Ok(Arc::new(UsernamePasswordCredential {
        client_id,
        username,
        password,
        authority: Authority::for_tenant(tenant)?,
        client: azcore::msal_client(),
    }))
}

fn managed_identity_credential(chained: bool) -> Result<Arc<dyn TokenSource>, String> {
    ManagedIdentitySource::new(ManagedIdentitySettings {
        client_id: variable(CLIENT_ENV),
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
            async fn token(&self, _scope: &str) -> Result<BearerToken, SourceError> {
                Ok(BearerToken {
                    secret: self.0.clone(),
                    expires_at: super::token_source::expiry(4_000_000_000),
                })
            }
        }

        Self::from_sources(vec![Source {
            name: "Fixed",
            credential: Ok(Arc::new(Fixed(secret.to_owned()))),
        }])
    }

    fn from_sources(sources: Vec<Source>) -> Self {
        Self {
            sources,
            selected: Mutex::new(None),
            cache: std::sync::Mutex::default(),
        }
    }

    pub async fn token(&self, scope: &str) -> Result<BearerToken, String> {
        if let Some(token) = self.cached(scope) {
            return Ok(token);
        }
        let token = self.acquire(scope).await?;
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(scope.to_owned(), token.clone());
        Ok(token)
    }

    async fn acquire(&self, scope: &str) -> Result<BearerToken, String> {
        let mut selected = self.selected.lock().await;
        match *selected {
            Some(index) => {
                drop(selected);
                self.source_token(index, scope)
                    .await
                    .map_err(|e| e.message().to_owned())
            }
            None => self.probe(scope, &mut selected).await,
        }
    }

    fn cached(&self, scope: &str) -> Option<BearerToken> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(scope)
            .filter(|token| token.expires_at > SystemTime::now() + CACHE_MARGIN)
            .cloned()
    }

    async fn probe(&self, scope: &str, selected: &mut Option<usize>) -> Result<BearerToken, String> {
        let mut failures = Vec::new();
        for index in 0..self.sources.len() {
            match self.source_token(index, scope).await {
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

    async fn source_token(&self, index: usize, scope: &str) -> Result<BearerToken, SourceError> {
        let source = &self.sources[index];
        match &source.credential {
            Err(e) => Err(SourceError::Unavailable(format!("{}: {e}", source.name))),
            Ok(credential) => credential.token(scope).await.map_err(|e| match e {
                SourceError::Unavailable(message) => SourceError::Unavailable(format!("{}: {message}", source.name)),
                failed @ SourceError::Failed(_) => failed,
            }),
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
        Unavailable,
        Failed,
    }

    struct Scripted {
        outcome: Outcome,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TokenSource for Scripted {
        async fn token(&self, scope: &str) -> Result<BearerToken, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.outcome {
                Outcome::Token(token) => Ok(BearerToken {
                    secret: format!("{token}@{scope}"),
                    expires_at: expiry(4_000_000_000),
                }),
                Outcome::Unavailable => Err(SourceError::Unavailable("nothing here".to_owned())),
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
            scripted("EnvironmentCredential", Outcome::Unavailable, &first),
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
        async fn token(&self, _scope: &str) -> Result<BearerToken, SourceError> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(BearerToken {
                    secret: "first".to_owned(),
                    expires_at: expiry(4_000_000_000),
                }),
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
            scripted("EnvironmentCredential", Outcome::Unavailable, &first),
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

    #[tokio::test]
    async fn a_chain_without_an_answer_lists_every_attempt() {
        let calls = counter();
        let chain = DefaultAzureCredential::from_sources(vec![
            Source {
                name: "EnvironmentCredential",
                credential: Err("missing environment variable AZURE_TENANT_ID".to_owned()),
            },
            scripted("ManagedIdentityCredential", Outcome::Unavailable, &calls),
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
