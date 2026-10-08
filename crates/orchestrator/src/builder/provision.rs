use std::collections::VecDeque;

use tokio::sync::mpsc::UnboundedReceiver;

use super::logs::ScopedLog;
use super::template_files::{PROVISION_EXIT_PREFIX, PROVISION_LOG_PREFIX};
use crate::proto::LogLevel;

const FAILURE_TAIL_LINES: usize = 30;
const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Default)]
struct Tail {
    lines: VecDeque<String>,
}

impl Tail {
    fn push(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        self.lines.push_back(trimmed.to_owned());
        if self.lines.len() > FAILURE_TAIL_LINES {
            self.lines.pop_front();
        }
    }

    fn failure(&self, status: &str) -> String {
        match self.lines.is_empty() {
            true => format!("exit status: {status}"),
            false => format!(
                "exit status: {status}; provisioning output tail:\n{}",
                self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
            ),
        }
    }
}

pub async fn wait_for_provisioning(log: &ScopedLog<'_>, mut console: UnboundedReceiver<String>) -> Result<(), String> {
    let mut tail = Tail::default();
    while let Some(raw) = console.recv().await {
        if let Some(text) = raw.strip_prefix(PROVISION_LOG_PREFIX) {
            log.push(LogLevel::Debug, text);
        }
        if raw.len() > MAX_LINE {
            return Err("bufio.Scanner: token too long".to_owned());
        }
        let line = raw.strip_suffix('\r').unwrap_or(&raw);
        if let Some(status) = line.strip_prefix(PROVISION_EXIT_PREFIX) {
            return match status {
                "0" => Ok(()),
                status => Err(tail.failure(status)),
            };
        }
        if let Some(text) = line.strip_prefix(PROVISION_LOG_PREFIX) {
            tail.push(text);
        }
    }
    Err("exit code not detected".to_owned())
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::builder::logs::BuildLog;

    async fn scan(lines: &[&str]) -> (Result<(), String>, Vec<String>) {
        let (sender, receiver) = unbounded_channel();
        lines.iter().for_each(|line| sender.send(line.to_string()).unwrap());
        drop(sender);
        let log = BuildLog::default();
        let fields = Default::default();
        let outcome = wait_for_provisioning(&log.scoped(&fields), receiver).await;
        let messages = log
            .query(&Default::default())
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        (outcome, messages)
    }

    #[tokio::test]
    async fn a_zero_exit_succeeds_even_with_a_carriage_return() {
        let (outcome, messages) = scan(&[
            "[    0.1] kernel",
            "[external] Starting provisioning script\r",
            "E2B_PROVISIONING_EXIT:0\r",
        ])
        .await;

        assert_eq!(outcome, Ok(()));
        assert_eq!(messages, vec!["Starting provisioning script\r"]);
    }

    #[tokio::test]
    async fn failures_carry_the_trimmed_tail() {
        let (with_tail, _) = scan(&[
            "[external] a",
            "[external]   ",
            "[external] b",
            "E2B_PROVISIONING_EXIT:1",
        ])
        .await;
        let (without_tail, _) = scan(&["E2B_PROVISIONING_EXIT:2"]).await;

        assert_eq!(
            with_tail,
            Err("exit status: 1; provisioning output tail:\na\nb".to_owned())
        );
        assert_eq!(without_tail, Err("exit status: 2".to_owned()));
    }

    #[tokio::test]
    async fn only_the_last_thirty_lines_are_kept() {
        let lines: Vec<String> = (0..31).map(|index| format!("[external] line {index}")).collect();
        let mut all: Vec<&str> = lines.iter().map(String::as_str).collect();
        all.push("E2B_PROVISIONING_EXIT:3");

        let (outcome, _) = scan(&all).await;

        let error = outcome.unwrap_err();
        assert!(error.starts_with("exit status: 3; provisioning output tail:\nline 1\n"));
        assert!(error.ends_with("\nline 30"));
    }

    #[tokio::test]
    async fn eof_and_markers_mid_line_are_not_an_exit() {
        let (outcome, _) = scan(&["echo E2B_PROVISIONING_EXIT:0", "[external] x"]).await;

        assert_eq!(outcome, Err("exit code not detected".to_owned()));
    }
}
