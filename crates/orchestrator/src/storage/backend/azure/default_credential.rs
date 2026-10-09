use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use azure_core::credentials::{Secret, TokenCredential};
use azure_identity::{
    ClientSecretCredential, DeveloperToolsCredential, ManagedIdentityCredential, ManagedIdentityCredentialOptions,
    UserAssignedId, WorkloadIdentityCredential,
};
use tokio::sync::Mutex;

const CHAIN_NAME: &str = "DefaultAzureCredential";
const TOKEN_CREDENTIALS_ENV: &str = "AZURE_TOKEN_CREDENTIALS";
const TENANT_ENV: &str = "AZURE_TENANT_ID";
const CLIENT_ENV: &str = "AZURE_CLIENT_ID";
const SECRET_ENV: &str = "AZURE_CLIENT_SECRET";
const CERTIFICATE_ENV: &str = "AZURE_CLIENT_CERTIFICATE_PATH";
const USERNAME_ENV: &str = "AZURE_USERNAME";
const PASSWORD_ENV: &str = "AZURE_PASSWORD";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerToken {
    pub secret: String,
    pub expires_at: SystemTime,
}

struct Source {
    name: &'static str,
    credential: Result<Arc<dyn TokenCredential>, String>,
}

pub struct DefaultAzureCredential {
    sources: Vec<Source>,
    selected: Mutex<Option<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Environment,
    WorkloadIdentity,
    ManagedIdentity,
    DeveloperTools,
}

const ALL: [Kind; 4] = [
    Kind::Environment,
    Kind::WorkloadIdentity,
    Kind::ManagedIdentity,
    Kind::DeveloperTools,
];
const PROD: [Kind; 3] = [Kind::Environment, Kind::WorkloadIdentity, Kind::ManagedIdentity];

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Environment => "EnvironmentCredential",
            Kind::WorkloadIdentity => "WorkloadIdentityCredential",
            Kind::ManagedIdentity => "ManagedIdentityCredential",
            Kind::DeveloperTools => "AzureCLICredential",
        }
    }

    fn build(self) -> Result<Arc<dyn TokenCredential>, String> {
        match self {
            Kind::Environment => environment_credential(),
            Kind::WorkloadIdentity => WorkloadIdentityCredential::new(None)
                .map(|credential| credential as Arc<dyn TokenCredential>)
                .map_err(|e| e.to_string()),
            Kind::ManagedIdentity => managed_identity_credential(),
            Kind::DeveloperTools => DeveloperToolsCredential::new(None)
                .map(|credential| credential as Arc<dyn TokenCredential>)
                .map_err(|e| e.to_string()),
        }
    }
}

fn selected_kinds(setting: Option<&str>) -> Result<Vec<Kind>, String> {
    match setting {
        None => Ok(ALL.to_vec()),
        Some("dev") => Ok(vec![Kind::DeveloperTools]),
        Some("prod") => Ok(PROD.to_vec()),
        Some(name) => ALL
            .into_iter()
            .find(|kind| kind.name().eq_ignore_ascii_case(name) || alias(*kind).eq_ignore_ascii_case(name))
            .map(|kind| vec![kind])
            .ok_or_else(|| {
                format!(
                    "invalid {TOKEN_CREDENTIALS_ENV} value {}. Valid values are \"dev\", \"prod\", or the name of any credential type in the default chain. See https://aka.ms/azsdk/go/identity/docs#DefaultAzureCredential for more information",
                    crate::go_strconv::quote(name)
                )
            }),
    }
}

fn alias(kind: Kind) -> &'static str {
    match kind {
        Kind::Environment => "EnvironmentCredential",
        Kind::WorkloadIdentity => "WorkloadIdentityCredential",
        Kind::ManagedIdentity => "ManagedIdentityCredential",
        Kind::DeveloperTools => "AzureDeveloperCLICredential",
    }
}

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn environment_credential() -> Result<Arc<dyn TokenCredential>, String> {
    let tenant = variable(TENANT_ENV).ok_or_else(|| format!("missing environment variable {TENANT_ENV}"))?;
    let client = variable(CLIENT_ENV).ok_or_else(|| format!("missing environment variable {CLIENT_ENV}"))?;
    match (variable(SECRET_ENV), variable(CERTIFICATE_ENV), variable(USERNAME_ENV)) {
        (Some(secret), _, _) => ClientSecretCredential::new(&tenant, client, Secret::new(secret), None)
            .map(|credential| credential as Arc<dyn TokenCredential>)
            .map_err(|e| e.to_string()),
        (None, Some(_), _) => Err(format!("{CERTIFICATE_ENV} is not supported, use {SECRET_ENV}")),
        (None, None, Some(_)) if variable(PASSWORD_ENV).is_none() => Err(format!("no value for {PASSWORD_ENV}")),
        (None, None, Some(_)) => Err(format!(
            "{USERNAME_ENV} and {PASSWORD_ENV} are not supported, use {SECRET_ENV}"
        )),
        (None, None, None) => Err(format!(
            "incomplete environment variable configuration. Only {TENANT_ENV} and {CLIENT_ENV} are set"
        )),
    }
}

fn managed_identity_credential() -> Result<Arc<dyn TokenCredential>, String> {
    let options = ManagedIdentityCredentialOptions {
        user_assigned_id: variable(CLIENT_ENV).map(UserAssignedId::ClientId),
        ..ManagedIdentityCredentialOptions::default()
    };
    ManagedIdentityCredential::new(Some(options))
        .map(|credential| credential as Arc<dyn TokenCredential>)
        .map_err(|e| e.to_string())
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
            Self::from_sources(
                kinds
                    .into_iter()
                    .map(|kind| Source {
                        name: kind.name(),
                        credential: kind.build(),
                    })
                    .collect(),
            )
        })
    }

    #[cfg(test)]
    pub fn with_static_token(secret: &str) -> Self {
        #[derive(Debug)]
        struct Fixed(String);

        #[async_trait::async_trait]
        impl TokenCredential for Fixed {
            async fn get_token(
                &self,
                _scopes: &[&str],
                _options: Option<azure_core::credentials::TokenRequestOptions<'_>>,
            ) -> azure_core::Result<azure_core::credentials::AccessToken> {
                Ok(azure_core::credentials::AccessToken::new(
                    self.0.clone(),
                    azure_core::time::OffsetDateTime::from_unix_timestamp(4_000_000_000).unwrap(),
                ))
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
        }
    }

    pub async fn token(&self, scope: &str) -> Result<BearerToken, String> {
        let known = *self.selected.lock().await;
        match known {
            Some(index) => self.source_token(index, scope).await,
            None => self.probe(scope).await,
        }
    }

    async fn probe(&self, scope: &str) -> Result<BearerToken, String> {
        let mut failures = Vec::new();
        for index in 0..self.sources.len() {
            match self.source_token(index, scope).await {
                Ok(token) => {
                    *self.selected.lock().await = Some(index);
                    return Ok(token);
                }
                Err(e) => failures.push(e),
            }
        }
        Err(chain_error(&failures))
    }

    async fn source_token(&self, index: usize, scope: &str) -> Result<BearerToken, String> {
        let source = &self.sources[index];
        let credential = source
            .credential
            .as_ref()
            .map_err(|e| format!("{}: {e}", source.name))?;
        credential
            .get_token(&[scope], None)
            .await
            .map(|token| BearerToken {
                secret: token.token.secret().to_owned(),
                expires_at: expiry(token.expires_on.unix_timestamp()),
            })
            .map_err(|e| format!("{}: {e}", source.name))
    }
}

fn expiry(unix_seconds: i64) -> SystemTime {
    u64::try_from(unix_seconds).map_or(UNIX_EPOCH, |seconds| UNIX_EPOCH + Duration::from_secs(seconds))
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

    use azure_core::credentials::AccessToken;
    use azure_core::time::OffsetDateTime;

    use super::*;

    #[derive(Debug)]
    struct Scripted {
        token: Option<&'static str>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TokenCredential for Scripted {
        async fn get_token(
            &self,
            scopes: &[&str],
            _options: Option<azure_core::credentials::TokenRequestOptions<'_>>,
        ) -> azure_core::Result<AccessToken> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.token {
                Some(token) => Ok(AccessToken::new(
                    format!("{token}@{}", scopes.join(",")),
                    OffsetDateTime::from_unix_timestamp(2_000_000_000).unwrap(),
                )),
                None => Err(azure_core::Error::with_message(
                    azure_core::error::ErrorKind::Credential,
                    "nothing here",
                )),
            }
        }
    }

    fn scripted(name: &'static str, token: Option<&'static str>, calls: &Arc<AtomicUsize>) -> Source {
        Source {
            name,
            credential: Ok(Arc::new(Scripted {
                token,
                calls: calls.clone(),
            })),
        }
    }

    #[tokio::test]
    async fn the_first_source_that_answers_wins_and_is_remembered() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let chain = DefaultAzureCredential::from_sources(vec![
            scripted("EnvironmentCredential", None, &first),
            scripted("ManagedIdentityCredential", Some("mi"), &second),
        ]);

        let token = chain.token("https://scope/.default").await.unwrap();
        chain.token("https://scope/.default").await.unwrap();

        assert_eq!(token.secret, "mi@https://scope/.default");
        assert_eq!(token.expires_at, UNIX_EPOCH + Duration::from_secs(2_000_000_000));
        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_chain_without_an_answer_lists_every_attempt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let chain = DefaultAzureCredential::from_sources(vec![
            Source {
                name: "EnvironmentCredential",
                credential: Err("missing environment variable AZURE_TENANT_ID".to_owned()),
            },
            scripted("ManagedIdentityCredential", None, &calls),
        ]);

        let error = chain.token("s").await.unwrap_err();

        assert!(error.starts_with("DefaultAzureCredential: failed to acquire a token.\nAttempted credentials:\n\tEnvironmentCredential: missing environment variable AZURE_TENANT_ID\n\tManagedIdentityCredential: "));
    }

    #[test]
    fn the_token_credentials_setting_selects_sources() {
        assert_eq!(selected_kinds(None).unwrap().len(), 4);
        assert_eq!(selected_kinds(Some("prod")).unwrap().len(), 3);
        assert_eq!(selected_kinds(Some("dev")).unwrap(), vec![Kind::DeveloperTools]);
        assert_eq!(
            selected_kinds(Some("managedidentitycredential")).unwrap(),
            vec![Kind::ManagedIdentity]
        );
        assert!(
            selected_kinds(Some("nope"))
                .unwrap_err()
                .starts_with("invalid AZURE_TOKEN_CREDENTIALS value \"nope\". Valid values are")
        );
    }
}
