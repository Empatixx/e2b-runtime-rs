use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::output::Output;
use super::registry::Processes;
use super::spawn::{OutputPipe, PIPE_CHUNK, PTY_CHUNK, Process, pump, signal_name};
use super::terminal::{HandoverFds, Terminal, Writer};
use crate::cgroups::ProcessType;
use crate::proto::process::__buffa::oneof::process_event::data_event::Output as Data;
use crate::proto::process::ProcessConfig;
use crate::proto::process::process_event::EndEvent;

pub struct Adoption {
    pub pid: u32,
    pub tag: Option<String>,
    pub config: ProcessConfig,
    pub process_type: ProcessType,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
    pub stdin: Option<OwnedFd>,
    pub tty: Option<OwnedFd>,
    pub timeout: Option<Duration>,
}

pub fn readopt(adoption: Adoption, processes: Arc<Processes>, thawed: watch::Receiver<u64>) -> Arc<Process> {
    let handover = HandoverFds {
        stdout: adoption.stdout.as_ref().map(AsRawFd::as_raw_fd),
        stderr: adoption.stderr.as_ref().map(AsRawFd::as_raw_fd),
        stdin: adoption.stdin.as_ref().map(AsRawFd::as_raw_fd),
        tty: adoption.tty.as_ref().map(AsRawFd::as_raw_fd),
    };
    let terminal = adoption.tty.as_ref().and_then(|tty| {
        let writer = File::from(tty.try_clone().ok()?);
        Some(Terminal::new(
            tty.as_raw_fd(),
            Box::pin(tokio::fs::File::from_std(writer)),
        ))
    });
    let pipes: Vec<OutputPipe> = match adoption.tty {
        Some(tty) => vec![pipe(tty, PTY_CHUNK, Data::Pty)],
        None => [
            (adoption.stdout, Data::Stdout as fn(Vec<u8>) -> Data),
            (adoption.stderr, Data::Stderr),
        ]
        .into_iter()
        .filter_map(|(fd, kind)| fd.map(|fd| pipe(fd, PIPE_CHUNK, kind)))
        .collect(),
    };
    let process = Arc::new(Process {
        pid: adoption.pid,
        tag: adoption.tag,
        config: adoption.config,
        output: Output::new(),
        end: watch::Sender::new(None),
        stdin: Mutex::new(
            adoption
                .stdin
                .map(|stdin| Box::pin(tokio::fs::File::from_std(File::from(stdin))) as Writer),
        ),
        pty: terminal,
        stop_output: CancellationToken::new(),
        handover,
        process_type: adoption.process_type,
        deadline: parking_lot::Mutex::new(None),
    });
    processes.add(process.clone());
    if let Some(timeout) = adoption.timeout {
        tokio::spawn(kill_after_thaw(process.clone(), timeout, thawed));
    }
    tokio::spawn(run(process.clone(), pipes, processes));
    process
}

fn pipe(fd: OwnedFd, chunk_size: usize, kind: fn(Vec<u8>) -> Data) -> OutputPipe {
    OutputPipe {
        reader: Box::new(tokio::fs::File::from_std(File::from(fd))),
        chunk_size,
        kind,
    }
}

async fn kill_after_thaw(process: Arc<Process>, timeout: Duration, mut thawed: watch::Receiver<u64>) {
    tokio::select! {
        _ = thawed.changed() => {}
        () = process.stop_output.cancelled() => return,
    }
    *process.deadline.lock() = Some(Instant::now() + timeout);
    tokio::select! {
        () = tokio::time::sleep(timeout) => drop(process.signal(Signal::SIGKILL)),
        () = process.stop_output.cancelled() => {}
    }
}

async fn run(process: Arc<Process>, pipes: Vec<OutputPipe>, processes: Arc<Processes>) {
    let mut readers: JoinSet<()> = pipes.into_iter().map(|pipe| pump(process.clone(), pipe)).collect();
    tokio::select! {
        _ = async { while readers.join_next().await.is_some() {} } => {}
        () = process.stop_output.cancelled() => {}
    }
    readers.abort_all();
    process.output.close();
    let pid = Pid::from_raw(process.pid as i32);
    let waited = tokio::task::spawn_blocking(move || waitpid(pid, None))
        .await
        .unwrap_or(Err(nix::errno::Errno::EINTR));
    let end = adopted_end(waited);
    tracing::info!(event_type = "process_end_readopted", pid = process.pid, process_result = ?end, "re-adopted process ended");
    process.end.send_replace(Some(end.clone()));
    processes.finish(&process, end);
}

fn adopted_end(waited: nix::Result<WaitStatus>) -> EndEvent {
    match waited {
        Ok(WaitStatus::Exited(_, code)) => EndEvent {
            exited: true,
            exit_code: code,
            status: "exited".to_owned(),
            ..Default::default()
        },
        Ok(WaitStatus::Signaled(_, signal, _)) => EndEvent {
            exit_code: 128 + signal as i32,
            status: signal_name(signal as i32),
            ..Default::default()
        },
        Ok(_) => EndEvent::default(),
        Err(errno) => EndEvent {
            error: Some(crate::util::fs::reason(&std::io::Error::from(errno))),
            status: "wait4 error".to_owned(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopted_processes_end_like_upstream_readopt() {
        let pid = Pid::from_raw(1);
        assert_eq!(
            adopted_end(Ok(WaitStatus::Exited(pid, 3))),
            EndEvent {
                exited: true,
                exit_code: 3,
                status: "exited".to_owned(),
                ..Default::default()
            }
        );
        let killed = adopted_end(Ok(WaitStatus::Signaled(pid, Signal::SIGKILL, false)));
        assert_eq!(
            (killed.exit_code, killed.status.as_str(), killed.exited),
            (137, "killed", false)
        );
        let failed = adopted_end(Err(nix::errno::Errno::ECHILD));
        assert_eq!(failed.status, "wait4 error");
        assert_eq!(failed.error.as_deref(), Some("no child processes"));
    }
}
