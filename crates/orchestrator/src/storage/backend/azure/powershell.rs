use std::process::Output;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Mutex;

use super::token_source::{BearerToken, SourceError, TokenSource, expiry};

const CREDENTIAL: &str = "AzurePowerShellCredential";
const MODULE_NOT_FOUND: &str = "Az.Accounts module not found";
const TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SUFFIX: &str = "/.default";
const SCRIPT: &str = include_str!("powershell/script.ps1");

pub struct PowerShellCredential {
    shell: String,
    lock: Mutex<()>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PowerShellToken {
    token: String,
    expires_on: i64,
}

impl Default for PowerShellCredential {
    fn default() -> Self {
        Self::with_shell("/bin/sh")
    }
}

impl PowerShellCredential {
    fn with_shell(shell: &str) -> Self {
        Self {
            shell: shell.to_owned(),
            lock: Mutex::new(()),
        }
    }

    async fn run(&self, command: &str) -> Result<Vec<u8>, String> {
        let child = Command::new(&self.shell)
            .args(["-c", command])
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .output();
        let output = tokio::time::timeout(TIMEOUT, child)
            .await
            .map_err(|_| "signal: killed".to_owned())?
            .map_err(|e| e.to_string())?;
        outcome(output)
    }
}

fn outcome(output: Output) -> Result<Vec<u8>, String> {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match output.status.success() {
        true => Ok(output.stdout),
        false if output.status.code() == Some(127) || stderr.contains("' is not recognized") => {
            Err("executable not found on path".to_owned())
        }
        false => Err(failure_message(&stderr, &status_text(&output))),
    }
}

fn status_text(output: &Output) -> String {
    output
        .status
        .code()
        .map_or_else(|| "signal: killed".to_owned(), |code| format!("exit status {code}"))
}

fn failure_message(stderr: &str, status: &str) -> String {
    match (
        stderr.contains(MODULE_NOT_FOUND),
        stderr.contains("Connect-AzAccount"),
        stderr.is_empty(),
    ) {
        (true, _, _) => MODULE_NOT_FOUND.to_owned(),
        (false, true, _) => "Please run \"Connect-AzAccount\" to set up an account".to_owned(),
        (false, false, true) => status.to_owned(),
        (false, false, false) => stderr.to_owned(),
    }
}

fn valid_scope(scope: &str) -> bool {
    scope
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | ':'))
}

fn utf16le_base64(text: &str) -> String {
    STANDARD.encode(text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>())
}

fn command_for(scope: &str) -> String {
    let resource = scope.strip_suffix(DEFAULT_SUFFIX).unwrap_or(scope);
    let script = SCRIPT
        .replace("{MODULE_NOT_FOUND}", MODULE_NOT_FOUND)
        .replace("{RESOURCE}", resource)
        .replace("{TENANT}", "");
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
    async fn token(&self, scope: &str) -> Result<BearerToken, SourceError> {
        if !valid_scope(scope) {
            return Err(SourceError::Failed(format!(
                "{CREDENTIAL}.GetToken(): invalid scope {}",
                crate::go_strconv::quote(scope)
            )));
        }
        let _serial = self.lock.lock().await;
        let stdout = self.run(&command_for(scope)).await.map_err(unavailable)?;
        serde_json::from_slice::<PowerShellToken>(&stdout)
            .map(|token| BearerToken {
                secret: token.token,
                expires_at: expiry(token.expires_on),
            })
            .map_err(unavailable)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn fake_shell(body: &str) -> (tempfile::TempDir, PowerShellCredential) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let credential = PowerShellCredential::with_shell(path.to_str().unwrap());
        (directory, credential)
    }

    #[test]
    fn the_command_encodes_the_script_as_utf16_little_endian() {
        assert_eq!(utf16le_base64("a\u{e9}"), "YQDpAA==");
        let command = command_for("https://storage.azure.com/.default");
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
        assert!(script.contains("$tenantId = ''"));
    }

    #[test]
    fn failures_are_summarised_like_the_sdk() {
        assert_eq!(
            failure_message("x Az.Accounts module not found y", "exit status 1"),
            MODULE_NOT_FOUND
        );
        assert_eq!(
            failure_message("Run Connect-AzAccount", "exit status 1"),
            "Please run \"Connect-AzAccount\" to set up an account"
        );
        assert_eq!(failure_message("", "exit status 1"), "exit status 1");
        assert_eq!(failure_message("boom", "exit status 1"), "boom");
    }

    #[test]
    fn scopes_are_restricted_to_url_characters() {
        assert!(valid_scope("https://storage.azure.com/.default"));
        assert!(!valid_scope("a b"));
        assert!(!valid_scope("a'b"));
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
