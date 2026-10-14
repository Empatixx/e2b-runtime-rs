use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe::Receiver;
use tokio::process::Child;
use tokio::time::{Instant, sleep_until};

const WAIT_DELAY: Duration = Duration::from_secs(2);
const WAIT_DELAY_EXPIRED: &str = "exec: WaitDelay expired before I/O complete";

#[derive(Debug)]
pub struct CommandFailure {
    pub error: String,
    pub output: String,
}

pub async fn combined_output(program: &str, args: &[&str]) -> Result<String, CommandFailure> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| CommandFailure {
            error: match e.kind() {
                std::io::ErrorKind::NotFound => format!("exec: {program:?}: executable file not found in $PATH"),
                _ => crate::util::errno::message(&e),
            },
            output: String::new(),
        })?;
    let combined = [output.stdout, output.stderr].concat();
    let text = String::from_utf8_lossy(&combined).into_owned();
    match output.status.code() {
        Some(0) => Ok(text),
        Some(code) => Err(CommandFailure {
            error: format!("exit status {code}"),
            output: text,
        }),
        None => Err(CommandFailure {
            error: "signal: killed".to_owned(),
            output: text,
        }),
    }
}

struct GroupGuard {
    group: Option<Pid>,
}

impl GroupGuard {
    fn of(child: &Child) -> Self {
        Self {
            group: child.id().map(|pid| Pid::from_raw(pid as i32)),
        }
    }

    fn kill(&self) {
        if let Some(group) = self.group {
            let _ = kill(Pid::from_raw(-group.as_raw()), Signal::SIGKILL).or_else(|_| kill(group, Signal::SIGKILL));
        }
    }

    fn disarm(&mut self) {
        self.group = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Default)]
struct Exchange {
    status: Option<std::io::Result<std::process::ExitStatus>>,
    output: Vec<u8>,
    drained: bool,
    overran: bool,
}

impl Exchange {
    fn settled(&self) -> bool {
        self.status.is_some() && self.drained
    }

    async fn abandon(mut self, child: &mut Child) -> Self {
        self.overran = !self.drained;
        if self.status.is_none() {
            let _ = child.start_kill();
            self.status = Some(child.wait().await);
        }
        self
    }
}

pub async fn group_output(deadline: Instant, program: &str, args: &[&str]) -> Result<String, CommandFailure> {
    let (reader, writer) = std::io::pipe().map_err(|e| io_failure(program, &e))?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(writer.try_clone().map_err(|e| io_failure(program, &e))?)
        .stderr(writer)
        .process_group(0);
    let mut child = command.spawn().map_err(|e| io_failure(program, &e))?;
    drop(command);
    let mut guard = GroupGuard::of(&child);
    let mut receiver = Receiver::from_owned_fd(reader.into()).map_err(|e| io_failure(program, &e))?;
    let exchange = converse(&mut child, &mut receiver, &guard, deadline).await;
    guard.disarm();
    conclude(exchange)
}

async fn converse(child: &mut Child, receiver: &mut Receiver, guard: &GroupGuard, deadline: Instant) -> Exchange {
    let mut exchange = Exchange::default();
    let mut cancel_at = Some(deadline);
    let mut give_up_at: Option<Instant> = None;
    while !exchange.settled() {
        tokio::select! {
            _ = receiver.read_to_end(&mut exchange.output), if !exchange.drained => exchange.drained = true,
            status = child.wait(), if exchange.status.is_none() => {
                exchange.status = Some(status);
                give_up_at.get_or_insert(Instant::now() + WAIT_DELAY);
            }
            () = sleep_until(cancel_at.unwrap_or(deadline)), if cancel_at.is_some() => {
                cancel_at = None;
                guard.kill();
                give_up_at.get_or_insert(Instant::now() + WAIT_DELAY);
            }
            () = sleep_until(give_up_at.unwrap_or(deadline)), if give_up_at.is_some() => {
                return exchange.abandon(child).await;
            }
        }
    }
    exchange
}

fn conclude(exchange: Exchange) -> Result<String, CommandFailure> {
    let text = String::from_utf8_lossy(&exchange.output).into_owned();
    let failure = |error: String| CommandFailure {
        error,
        output: text.clone(),
    };
    match exchange.status {
        Some(Ok(status)) if status.success() && exchange.overran => Err(failure(WAIT_DELAY_EXPIRED.to_owned())),
        Some(Ok(status)) if status.success() => Ok(text),
        Some(Ok(status)) => Err(failure(match status.code() {
            Some(code) => format!("exit status {code}"),
            None => "signal: killed".to_owned(),
        })),
        Some(Err(e)) => Err(failure(crate::util::errno::message(&e))),
        None => Err(failure("signal: killed".to_owned())),
    }
}

fn io_failure(program: &str, error: &std::io::Error) -> CommandFailure {
    CommandFailure {
        error: match error.kind() {
            std::io::ErrorKind::NotFound => format!("exec: {program:?}: executable file not found in $PATH"),
            _ => crate::util::errno::message(error),
        },
        output: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::paths::look_path;

    fn within(duration: Duration) -> Instant {
        Instant::now() + duration
    }

    #[tokio::test]
    async fn a_successful_command_returns_stdout_and_stderr_in_order() {
        let output = group_output(
            within(Duration::from_secs(5)),
            "sh",
            &["-c", "echo out; echo err >&2; echo again"],
        )
        .await
        .unwrap();

        assert_eq!(output, "out\nerr\nagain\n");
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_status_and_output() {
        let failure = group_output(within(Duration::from_secs(5)), "sh", &["-c", "echo why >&2; exit 3"])
            .await
            .unwrap_err();

        assert_eq!(
            (failure.error.as_str(), failure.output.as_str()),
            ("exit status 3", "why\n")
        );
    }

    #[tokio::test]
    async fn a_missing_program_reads_like_go() {
        let failure = group_output(within(Duration::from_secs(5)), "no-such-program-anywhere", &[])
            .await
            .unwrap_err();

        assert_eq!(
            failure.error,
            "exec: \"no-such-program-anywhere\": executable file not found in $PATH"
        );
    }

    #[tokio::test]
    async fn cancelling_kills_the_forked_helpers_too() {
        let started = Instant::now();

        let failure = group_output(within(Duration::from_millis(100)), "sh", &["-c", "sleep 30 & wait"])
            .await
            .unwrap_err();

        assert_eq!(failure.error, "signal: killed");
        assert!(
            started.elapsed() < WAIT_DELAY,
            "the forked helper outlived the cancel and held the pipes open"
        );
    }

    #[tokio::test]
    async fn the_wait_is_bounded_when_a_helper_escapes_the_group() {
        if look_path("setsid").is_none() {
            return;
        }
        let started = Instant::now();

        let failure = group_output(
            within(Duration::from_millis(100)),
            "sh",
            &["-c", "setsid sleep 30 & wait"],
        )
        .await
        .unwrap_err();

        assert_eq!(failure.error, "signal: killed");
        assert!(
            started.elapsed() < WAIT_DELAY + Duration::from_secs(5),
            "the wait outlived an escaped helper's pipes"
        );
    }

    #[tokio::test]
    async fn an_exited_command_whose_helper_holds_the_pipes_overruns_the_wait_delay() {
        let started = Instant::now();

        let failure = group_output(within(Duration::from_secs(30)), "sh", &["-c", "sleep 30 & exit 0"])
            .await
            .unwrap_err();

        assert_eq!(failure.error, WAIT_DELAY_EXPIRED);
        assert!(started.elapsed() < WAIT_DELAY + Duration::from_secs(5));
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_process_group() {
        let marker = tempfile::tempdir().unwrap().keep().join("survived");
        let script = format!("sleep 1; touch {}", marker.display());

        let args = ["-c", script.as_str()];
        let running = group_output(within(Duration::from_secs(30)), "sh", &args);
        let _ = tokio::time::timeout(Duration::from_millis(100), running).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;

        assert!(!marker.exists(), "the dropped command kept running");
    }
}
