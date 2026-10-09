use chrono::{Local, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::dev_tool::{ShellExec, valid_scope};
use super::tenant::TenantPolicy;
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource, expiry};

const CREDENTIAL: &str = "AzureCLICredential";
const DEFAULT_SUFFIX: &str = "/.default";
const EXPIRES_ON_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";

pub struct AzureCliCredential {
    shell: ShellExec,
    tenants: TenantPolicy,
    lock: Mutex<()>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CliToken {
    #[serde(rename = "accessToken")]
    access_token: String,
    expires_on: i64,
    #[serde(rename = "expiresOn")]
    expires_on_text: String,
}

impl AzureCliCredential {
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
    let resource = scope.strip_suffix(DEFAULT_SUFFIX).unwrap_or(scope);
    let mut command = format!("az account get-access-token -o json --resource {resource}");
    if !tenant.is_empty() {
        command.push_str(&format!(" --tenant {tenant}"));
    }
    command
}

fn bearer(output: &[u8]) -> Result<BearerToken, String> {
    let token: CliToken = serde_json::from_slice(output).map_err(|e| e.to_string())?;
    let expires_at = match token.expires_on {
        0 => local_expiry(&token.expires_on_text)?,
        seconds => expiry(seconds),
    };
    Ok(BearerToken::without_refresh(token.access_token, expires_at))
}

fn local_expiry(text: &str) -> Result<std::time::SystemTime, String> {
    NaiveDateTime::parse_from_str(text, EXPIRES_ON_FORMAT)
        .ok()
        .and_then(|naive| Local.from_local_datetime(&naive).earliest())
        .map(|local| std::time::SystemTime::from(local.with_timezone(&Utc)))
        .ok_or_else(|| {
            format!(
                "{CREDENTIAL}: error parsing token expiration time {}: parsing time {} as \"2006-01-02 15:04:05.999999\": cannot parse {} as \"2006\"",
                crate::go_strconv::quote(text),
                crate::go_strconv::quote(text),
                crate::go_strconv::quote(text)
            )
        })
}

#[async_trait::async_trait]
impl TokenSource for AzureCliCredential {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
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
            .map_err(|failure| SourceError::Unavailable(format!("{CREDENTIAL}: {}", failure.message())))?;
        bearer(&output).map_err(SourceError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::super::dev_tool::fake_shell;
    use super::*;

    fn credential(body: &str) -> (tempfile::TempDir, AzureCliCredential) {
        let (directory, shell) = fake_shell(body);
        (
            directory,
            AzureCliCredential::with_shell(shell, TenantPolicy::new("", &[])),
        )
    }

    #[test]
    fn commands_use_the_v1_resource_and_the_tenant() {
        assert_eq!(
            command_for("https://storage.azure.com/.default", ""),
            "az account get-access-token -o json --resource https://storage.azure.com"
        );
        assert_eq!(
            command_for("https://storage.azure.com/.default", "tid"),
            "az account get-access-token -o json --resource https://storage.azure.com --tenant tid"
        );
    }

    #[test]
    fn tokens_read_expires_on_seconds_or_the_local_time_string() {
        let seconds = bearer(br#"{"accessToken":"t","expires_on":4000000000,"expiresOn":"ignored"}"#).unwrap();
        let text = bearer(br#"{"accessToken":"t","expiresOn":"2100-01-02 03:04:05.123456"}"#).unwrap();

        assert_eq!(seconds.expires_at, expiry(4_000_000_000));
        assert!(text.expires_at > expiry(4_000_000_000));
        assert!(
            bearer(br#"{"accessToken":"t","expiresOn":"soon"}"#)
                .unwrap_err()
                .starts_with("AzureCLICredential: error parsing token expiration time \"soon\": parsing time \"soon\"")
        );
    }

    #[tokio::test]
    async fn a_token_is_read_from_the_cli_output() {
        let (_directory, credential) = credential(r#"echo '{"accessToken":"cli","expires_on":4000000000}'"#);

        let token = credential.token("https://storage.azure.com/.default").await.unwrap();

        assert_eq!(token.secret, "cli");
    }

    #[tokio::test]
    async fn failures_make_the_credential_unavailable() {
        let (_a, missing) = credential("exit 127");
        let (_b, failing) = credential("echo 'please run az login' >&2; exit 1");

        assert_eq!(
            missing.token("r").await.unwrap_err(),
            SourceError::Unavailable("AzureCLICredential: executable not found on path".to_owned())
        );
        assert_eq!(
            failing.token("r").await.unwrap_err(),
            SourceError::Unavailable("AzureCLICredential: please run az login\n".to_owned())
        );
    }

    #[tokio::test]
    async fn bad_scopes_and_tenants_stop_the_chain_before_the_cli_runs() {
        let (_directory, credential) = credential("exit 0");

        assert_eq!(
            credential.token("a b").await.unwrap_err(),
            SourceError::Failed("AzureCLICredential.GetToken(): invalid scope \"a b\"".to_owned())
        );
        assert!(matches!(
            credential
                .token_for(&TokenRequest::new("r").with_tenant("bad tenant"))
                .await
                .unwrap_err(),
            SourceError::Failed(_)
        ));
    }
}
