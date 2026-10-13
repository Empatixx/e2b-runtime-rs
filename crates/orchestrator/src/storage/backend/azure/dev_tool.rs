use std::process::Output;
use std::time::Duration;

use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(10);
const WORKING_DIRECTORY: &str = "/bin";
const NOT_FOUND_EXIT_CODE: i32 = 127;
const NOT_RECOGNIZED: &str = "' is not recognized";
const TIMED_OUT: &str = "timed out";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFailure {
    NotFound,
    Failed { stderr: String, status: String },
}

pub struct ShellExec {
    shell: String,
    timeout: Duration,
}

impl Default for ShellExec {
    fn default() -> Self {
        Self::with_shell("/bin/sh")
    }
}

impl ShellExec {
    pub fn with_shell(shell: &str) -> Self {
        Self {
            shell: shell.to_owned(),
            timeout: TIMEOUT,
        }
    }

    pub async fn run(&self, command: &str) -> Result<Vec<u8>, ExecFailure> {
        let child = Command::new(&self.shell)
            .args(["-c", command])
            .current_dir(WORKING_DIRECTORY)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .output();
        let output = tokio::time::timeout(self.timeout, child)
            .await
            .map_err(|_| failed(String::new(), TIMED_OUT.to_owned()))?
            .map_err(|e| failed(String::new(), e.to_string()))?;
        outcome(output)
    }
}

impl ExecFailure {
    pub fn message(&self) -> String {
        match self {
            Self::NotFound => "executable not found on path".to_owned(),
            Self::Failed { stderr, status } => match stderr.is_empty() {
                true => status.clone(),
                false => stderr.clone(),
            },
        }
    }
}

fn failed(stderr: String, status: String) -> ExecFailure {
    ExecFailure::Failed { stderr, status }
}

fn outcome(output: Output) -> Result<Vec<u8>, ExecFailure> {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match output.status.success() {
        true => Ok(output.stdout),
        false if output.status.code() == Some(NOT_FOUND_EXIT_CODE) || stderr.contains(NOT_RECOGNIZED) => {
            Err(ExecFailure::NotFound)
        }
        false => Err(failed(stderr, output.status.to_string())),
    }
}

pub fn valid_scope(scope: &str) -> bool {
    scope
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | ':'))
}

#[cfg(test)]
pub fn fake_shell(body: &str) -> (tempfile::TempDir, ShellExec) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    while std::process::Command::new(&path)
        .args(["-c", ":"])
        .output()
        .is_err_and(|e| e.raw_os_error() == Some(nix::libc::ETXTBSY))
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    let shell = ShellExec {
        timeout: Duration::from_secs(120),
        ..ShellExec::with_shell(path.to_str().unwrap())
    };
    (directory, shell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successful_commands_return_stdout() {
        let (_directory, shell) = fake_shell("echo out");

        assert_eq!(shell.run("ignored").await.unwrap(), b"out\n");
    }

    #[tokio::test]
    async fn exit_127_and_unrecognized_commands_mean_the_executable_is_missing() {
        let (_a, exit) = fake_shell("exit 127");
        let (_b, windows) = fake_shell("echo \"'az' is not recognized\" >&2; exit 1");

        assert_eq!(exit.run("x").await.unwrap_err(), ExecFailure::NotFound);
        assert_eq!(windows.run("x").await.unwrap_err(), ExecFailure::NotFound);
        assert_eq!(ExecFailure::NotFound.message(), "executable not found on path");
    }

    #[tokio::test]
    async fn failures_carry_stderr_or_the_exit_status() {
        let (_a, with_stderr) = fake_shell("echo boom >&2; exit 2");
        let (_b, silent) = fake_shell("exit 3");

        assert_eq!(with_stderr.run("x").await.unwrap_err().message(), "boom\n");
        assert_eq!(silent.run("x").await.unwrap_err().message(), "exit status: 3");
    }

    #[tokio::test]
    async fn commands_run_in_bin() {
        let (_directory, shell) = fake_shell("pwd");

        let printed = String::from_utf8(shell.run("x").await.unwrap()).unwrap();
        assert_eq!(
            std::fs::canonicalize(printed.trim()).unwrap(),
            std::fs::canonicalize("/bin").unwrap()
        );
    }

    #[test]
    fn scopes_are_restricted_to_url_characters() {
        assert!(valid_scope("https://storage.azure.com/.default"));
        assert!(!valid_scope("a b"));
        assert!(!valid_scope("a'b"));
    }
}
