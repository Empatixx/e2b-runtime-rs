use chrono::NaiveDateTime;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::dev_tool::{ExecFailure, ShellExec, valid_scope};
use super::tenant::TenantPolicy;
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource};

const CREDENTIAL: &str = "AzureDeveloperCLICredential";
const EXPIRES_ON_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

pub struct AzureDeveloperCliCredential {
    shell: ShellExec,
    tenants: TenantPolicy,
    lock: Mutex<()>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DeveloperCliToken {
    token: String,
    #[serde(rename = "expiresOn")]
    expires_on: String,
}

impl AzureDeveloperCliCredential {
    pub fn new(tenants: TenantPolicy) -> Self {
        Self::with_shell(ShellExec::default(), tenants)
    }

    fn with_shell(shell: ShellExec, tenants: TenantPolicy) -> Self {
        Self {
            shell,
            tenants,
            lock: Mutex::new(()),
        }
    }
}

fn command_for(scope: &str, tenant: &str) -> String {
    let mut command = format!("azd auth token -o json --no-prompt --scope {scope}");
    if !tenant.is_empty() {
        command.push_str(&format!(" --tenant-id {tenant}"));
    }
    command
}

fn bearer(output: &[u8]) -> Result<BearerToken, String> {
    let token: DeveloperCliToken = serde_json::from_slice(output).map_err(|e| e.to_string())?;
    let expires_at = NaiveDateTime::parse_from_str(&token.expires_on, EXPIRES_ON_FORMAT)
        .map(|naive| std::time::SystemTime::from(naive.and_utc()))
        .map_err(|_| {
            let quoted = crate::go_strconv::quote(&token.expires_on);
            format!(
                "error parsing token expiration time {quoted}: parsing time {quoted} as \"2006-01-02T15:04:05Z\": cannot parse {quoted} as \"2006\""
            )
        })?;
    Ok(BearerToken::without_refresh(token.token, expires_at))
}

fn unavailable(failure: &ExecFailure) -> SourceError {
    let message = format!("{CREDENTIAL}: {}", failure.message());
    match message.contains("azd auth login") {
        true => SourceError::Unavailable(format!(
            "{CREDENTIAL}: please run \"azd auth login\" from a command prompt to authenticate before using this credential"
        )),
        false => SourceError::Unavailable(message),
    }
}

#[async_trait::async_trait]
impl TokenSource for AzureDeveloperCliCredential {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        if request.scope.is_empty() {
            return Err(SourceError::Failed(format!(
                "{CREDENTIAL}: GetToken() requires at least one scope"
            )));
        }
        if !valid_scope(request.scope) {
            return Err(SourceError::Failed(format!(
                "{CREDENTIAL}.GetToken(): invalid scope {}",
                crate::go_strconv::quote(request.scope)
            )));
        }
        let tenant = self
            .tenants
            .resolve(CREDENTIAL, request.tenant)
            .map_err(SourceError::Failed)?;
        let _serial = self.lock.lock().await;
        let output = self
            .shell
            .run(&command_for(request.scope, &tenant))
            .await
            .map_err(|failure| unavailable(&failure))?;
        bearer(&output).map_err(SourceError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::super::dev_tool::fake_shell;
    use super::*;

    fn credential(body: &str) -> (tempfile::TempDir, AzureDeveloperCliCredential) {
        let (directory, shell) = fake_shell(body);
        (
            directory,
            AzureDeveloperCliCredential::with_shell(shell, TenantPolicy::new("", &[])),
        )
    }

    #[test]
    fn commands_pass_the_scope_unchanged_and_the_tenant() {
        assert_eq!(
            command_for("https://storage.azure.com/.default", ""),
            "azd auth token -o json --no-prompt --scope https://storage.azure.com/.default"
        );
        assert_eq!(
            command_for("s", "tid"),
            "azd auth token -o json --no-prompt --scope s --tenant-id tid"
        );
    }

    #[tokio::test]
    async fn a_token_is_read_from_the_azd_output() {
        let (_directory, credential) = credential(r#"echo '{"token":"azd","expiresOn":"2100-01-02T03:04:05Z"}'"#);

        let token = credential.token("s").await.unwrap();

        assert_eq!(token.secret, "azd");
        assert!(token.expires_at > super::super::token_source::expiry(4_000_000_000));
    }

    #[tokio::test]
    async fn an_unexpected_expiry_format_is_unavailable() {
        let (_directory, credential) = credential(r#"echo '{"token":"azd","expiresOn":"soon"}'"#);

        let error = credential.token("s").await.unwrap_err();

        assert!(
            matches!(&error, SourceError::Unavailable(text) if text.starts_with("error parsing token expiration time \"soon\": parsing time"))
        );
    }

    #[tokio::test]
    async fn a_missing_login_is_reported_in_azds_words() {
        let (_directory, credential) = credential("echo 'ERROR: run azd auth login first' >&2; exit 1");

        assert_eq!(
            credential.token("s").await.unwrap_err(),
            SourceError::Unavailable(
                "AzureDeveloperCLICredential: please run \"azd auth login\" from a command prompt to authenticate before using this credential".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn a_missing_binary_is_unavailable() {
        let (_directory, credential) = credential("exit 127");

        assert_eq!(
            credential.token("s").await.unwrap_err(),
            SourceError::Unavailable("AzureDeveloperCLICredential: executable not found on path".to_owned())
        );
    }

    #[tokio::test]
    async fn invalid_scopes_stop_the_chain() {
        let (_directory, credential) = credential("exit 0");

        assert_eq!(
            credential.token("a b").await.unwrap_err(),
            SourceError::Failed("AzureDeveloperCLICredential.GetToken(): invalid scope \"a b\"".to_owned())
        );
    }
}
