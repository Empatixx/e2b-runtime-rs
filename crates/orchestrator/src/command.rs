use std::ffi::OsStr;
use std::io;
use std::process::{ExitStatus, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFailure {
    pub message: String,
    pub exit_code: Option<i32>,
    pub output: Vec<u8>,
}

pub struct Command {
    command: tokio::process::Command,
    program: String,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let mut command = tokio::process::Command::new(program.as_ref());
        command.kill_on_drop(true).stdin(Stdio::null());
        Self {
            command,
            program: program.as_ref().to_string_lossy().into_owned(),
        }
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.command.env(key, value);
        self
    }

    pub fn current_dir(mut self, dir: &str) -> Self {
        self.command.current_dir(dir);
        self
    }

    pub async fn run(mut self) -> Result<Vec<u8>, CommandFailure> {
        let output = self
            .command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| start_failure(&self.program, &e))?;
        checked(output.status, output.stdout)
    }

    pub async fn combined_output(mut self) -> Result<Vec<u8>, CommandFailure> {
        let (reader, writer) = std::io::pipe().map_err(|e| start_failure(&self.program, &e))?;
        let error_writer = writer.try_clone().map_err(|e| start_failure(&self.program, &e))?;
        let mut child = self
            .command
            .stdout(writer)
            .stderr(error_writer)
            .spawn()
            .map_err(|e| start_failure(&self.program, &e))?;
        drop(self.command);
        let output = tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut output = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut output).map(|_| output)
        });
        let status = child.wait().await.map_err(|e| start_failure(&self.program, &e))?;
        let output = output.await.ok().and_then(Result::ok).unwrap_or_default();
        checked(status, output)
    }
}

fn checked(status: ExitStatus, output: Vec<u8>) -> Result<Vec<u8>, CommandFailure> {
    match status.success() {
        true => Ok(output),
        false => Err(CommandFailure {
            message: status.to_string(),
            exit_code: status.code(),
            output,
        }),
    }
}

fn start_failure(program: &str, error: &io::Error) -> CommandFailure {
    CommandFailure {
        message: format!("{program}: {error}"),
        exit_code: None,
        output: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failures_carry_the_status_and_output() {
        let exit = Command::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .combined_output()
            .await;
        let killed = Command::new("sh").args(["-c", "kill -9 $$"]).run().await;
        let missing = Command::new("no-such-binary-e2b").run().await;

        let exit = exit.unwrap_err();
        assert_eq!((exit.message.as_str(), exit.exit_code), ("exit status: 3", Some(3)));
        assert_eq!(exit.output, b"out\nerr\n");
        assert_eq!(killed.unwrap_err().exit_code, None);
        assert!(missing.unwrap_err().message.starts_with("no-such-binary-e2b: "));
    }
}
