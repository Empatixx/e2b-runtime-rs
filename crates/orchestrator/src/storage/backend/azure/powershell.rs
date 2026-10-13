use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::dev_tool::{ExecFailure, ShellExec, valid_scope};
use super::tenant::TenantPolicy;
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource, expiry};

const CREDENTIAL: &str = "AzurePowerShellCredential";
const MODULE_NOT_FOUND: &str = "Az.Accounts module not found";
const DEFAULT_SUFFIX: &str = "/.default";
const SCRIPT: &str = include_str!("powershell/script.ps1");

pub struct PowerShellCredential {
    shell: ShellExec,
    tenants: TenantPolicy,
    lock: Mutex<()>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PowerShellToken {
    token: String,
    expires_on: i64,
}

impl PowerShellCredential {
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

fn failure_message(failure: &ExecFailure) -> String {
    match failure {
        ExecFailure::Failed { stderr, .. } if stderr.contains(MODULE_NOT_FOUND) => MODULE_NOT_FOUND.to_owned(),
        ExecFailure::Failed { stderr, .. } if stderr.contains("Connect-AzAccount") => {
            "Please run \"Connect-AzAccount\" to set up an account".to_owned()
        }
        other => other.message(),
    }
}

fn utf16le_base64(text: &str) -> String {
    STANDARD.encode(text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>())
}

fn command_for(scope: &str, tenant: &str) -> String {
    let resource = scope.strip_suffix(DEFAULT_SUFFIX).unwrap_or(scope);
    let script = SCRIPT
        .replace("{MODULE_NOT_FOUND}", MODULE_NOT_FOUND)
        .replace("{RESOURCE}", resource)
        .replace("{TENANT}", tenant);
    format!(
        "pwsh -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand {}",
        utf16le_base64(&script)
    )
}

fn unavailable(message: impl std::fmt::Display) -> SourceError {
    SourceError::Unavailable(format!("{CREDENTIAL}: {message}"))
}

#[async_trait::async_trait]
impl TokenSource for PowerShellCredential {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let scope = request.scope;
        if !valid_scope(scope) {
            return Err(SourceError::Failed(format!(
                "{CREDENTIAL}.GetToken(): invalid scope {scope:?}"
            )));
        }
        let tenant = self
            .tenants
            .resolve(CREDENTIAL, request.tenant)
            .map_err(SourceError::Failed)?;
        let _serial = self.lock.lock().await;
        let stdout = self
            .shell
            .run(&command_for(scope, &tenant))
            .await
            .map_err(|failure| unavailable(failure_message(&failure)))?;
        serde_json::from_slice::<PowerShellToken>(&stdout)
            .map(|token| BearerToken::without_refresh(token.token, expiry(token.expires_on)))
            .map_err(unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::super::dev_tool::fake_shell as fake;
    use super::*;

    fn fake_shell(body: &str) -> (tempfile::TempDir, PowerShellCredential) {
        let (directory, shell) = fake(body);
        (
            directory,
            PowerShellCredential::with_shell(shell, TenantPolicy::new("", &[])),
        )
    }

    #[test]
    fn the_command_encodes_the_script_as_utf16_little_endian() {
        assert_eq!(utf16le_base64("a\u{e9}"), "YQDpAA==");
        let command = command_for("https://storage.azure.com/.default", "tid");
        assert!(command.starts_with("pwsh -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand "));
        let encoded = command.rsplit(' ').next().unwrap();
        let bytes = STANDARD.decode(encoded).unwrap();
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let script = String::from_utf16(&units).unwrap();
        assert!(script.contains("ResourceUrl   = 'https://storage.azure.com'"));
        assert!(script.contains("Write-Error 'Az.Accounts module not found'"));
        assert!(script.contains("$tenantId = 'tid'"));
    }

    #[test]
    fn failures_are_summarised_like_the_sdk() {
        let failed = |stderr: &str| ExecFailure::Failed {
            stderr: stderr.to_owned(),
            status: "exit status: 1".to_owned(),
        };

        assert_eq!(
            failure_message(&failed("x Az.Accounts module not found y")),
            MODULE_NOT_FOUND
        );
        assert_eq!(
            failure_message(&failed("Run Connect-AzAccount")),
            "Please run \"Connect-AzAccount\" to set up an account"
        );
        assert_eq!(failure_message(&failed("")), "exit status: 1");
        assert_eq!(failure_message(&failed("boom")), "boom");
    }

    #[tokio::test]
    async fn other_tenants_need_the_allow_list() {
        let (_directory, credential) = fake_shell("exit 0");

        let error = credential
            .token_for(&TokenRequest::new("https://storage.azure.com/.default").with_tenant("bad tenant"))
            .await
            .unwrap_err();

        assert!(matches!(error, SourceError::Failed(_)));
    }

    #[tokio::test]
    async fn a_token_is_read_from_the_json_the_script_prints() {
        let (_directory, credential) = fake_shell(r#"echo '{"Token":"ps-token","ExpiresOn":4000000000}'"#);

        let token = credential.token("https://storage.azure.com/.default").await.unwrap();

        assert_eq!(token.secret, "ps-token");
        assert_eq!(token.expires_at, expiry(4_000_000_000));
    }

    #[tokio::test]
    async fn a_missing_executable_makes_the_credential_unavailable() {
        let (_directory, credential) = fake_shell("exit 127");

        assert_eq!(
            credential
                .token("https://storage.azure.com/.default")
                .await
                .unwrap_err(),
            SourceError::Unavailable("AzurePowerShellCredential: executable not found on path".to_owned())
        );
    }

    #[tokio::test]
    async fn a_script_failure_carries_its_stderr() {
        let (_directory, credential) = fake_shell("echo 'Connect-AzAccount needed' >&2; exit 1");

        assert_eq!(
            credential
                .token("https://storage.azure.com/.default")
                .await
                .unwrap_err(),
            SourceError::Unavailable(
                "AzurePowerShellCredential: Please run \"Connect-AzAccount\" to set up an account".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn invalid_scopes_stop_the_chain() {
        let (_directory, credential) = fake_shell("exit 0");

        assert_eq!(
            credential.token("a b").await.unwrap_err(),
            SourceError::Failed("AzurePowerShellCredential.GetToken(): invalid scope \"a b\"".to_owned())
        );
    }
}
