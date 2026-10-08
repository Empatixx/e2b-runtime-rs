use std::net::SocketAddr;

use super::logs::ScopedLog;
use crate::envd::process::{CommandContext, run_command};
use crate::proto::LogLevel;

pub struct CommandLog<'a> {
    pub log: &'a ScopedLog<'a>,
    pub level: LogLevel,
    pub id: &'a str,
}

pub async fn run_logged(
    envd: SocketAddr,
    command: &str,
    context: &CommandContext,
    output: &CommandLog<'_>,
) -> Result<(), String> {
    run_command(envd, command, context, |stdout, stderr| {
        log_stream(output, "stdout", stdout);
        log_stream(output, "stderr", stderr);
    })
    .await
}

pub async fn run_quiet(envd: SocketAddr, command: &str, context: &CommandContext) -> Result<(), String> {
    run_command(envd, command, context, |_, _| {}).await
}

pub async fn run_with_output(envd: SocketAddr, command: &str, context: &CommandContext) -> Result<Vec<Chunk>, String> {
    let mut chunks = Vec::new();
    run_command(envd, command, context, |stdout, stderr| {
        chunks.push(Chunk {
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        })
    })
    .await
    .map(|()| chunks)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub stdout: String,
    pub stderr: String,
}

pub const ROOT: &str = "root";

fn log_stream(output: &CommandLog, name: &str, content: &str) {
    content
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .for_each(|line| {
            output
                .log
                .push(output.level, format!("[{}] [{name}]: {line}", output.id))
        });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::builder::logs::BuildLog;

    #[test]
    fn output_lines_are_trimmed_and_blank_ones_dropped() {
        let log = BuildLog::default();
        let fields = BTreeMap::new();
        let scoped = log.scoped(&fields);
        let output = CommandLog {
            log: &scoped,
            level: LogLevel::Info,
            id: "builder 1/2",
        };

        log_stream(&output, "stdout", "  hello \n\n world\n");
        log_stream(&output, "stderr", "");

        let lines: Vec<String> = log
            .query(&Default::default())
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        assert_eq!(
            lines,
            vec!["[builder 1/2] [stdout]: hello", "[builder 1/2] [stdout]: world"]
        );
    }
}
