use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use connectrpc::{ConnectError, ErrorCode};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Gid, Pid, Uid};
use pty_process::{OwnedWritePty, Size};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::output::Output;
use super::registry::Processes;
use crate::cgroups::Placement;
use crate::proto::process::__buffa::oneof::process_event::data_event::Output as Data;
use crate::proto::process::process_event::{DataEvent, EndEvent};
use crate::proto::process::{ProcessConfig, StartRequest};
use crate::user::User;
use crate::util::fs::reason;
use crate::util::paths::expand_and_resolve;

const OOM_SCORE: i32 = 100;
const IO_CLASS: u32 = 2;
const IO_PRIORITY: u32 = 4;
const PIPE_CHUNK: usize = 32 << 10;
const PTY_CHUNK: usize = 16 << 10;

pub struct Process {
    pub pid: u32,
    pub tag: Option<String>,
    pub config: ProcessConfig,
    pub output: Output,
    pub end: watch::Sender<Option<EndEvent>>,
    pub stdin: Mutex<Option<ChildStdin>>,
    pub pty: Option<Mutex<OwnedWritePty>>,
    pub stop_output: CancellationToken,
}

impl Process {
    pub async fn write_stdin(&self, data: &[u8]) -> Result<(), String> {
        if self.pty.is_some() {
            return Err("tty assigned to process — input should be written to the pty, not the stdin".to_owned());
        }
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or("stdin not enabled or closed")?;
        stdin.write_all(data).await.map_err(|e| {
            format!(
                "error writing to stdin of process '{}': write |1: {}",
                self.pid,
                reason(&e)
            )
        })
    }

    pub async fn write_pty(&self, data: &[u8]) -> Result<(), String> {
        let pty = self
            .pty
            .as_ref()
            .ok_or("tty not assigned to process — input should be written to the stdin, not the tty")?;
        pty.lock().await.write_all(data).await.map_err(|e| {
            format!(
                "error writing to tty of process '{}': write /dev/ptmx: {}",
                self.pid,
                reason(&e)
            )
        })
    }

    pub async fn close_stdin(&self) -> Result<(), String> {
        if self.pty.is_some() {
            return Err("cannot close stdin for PTY process — send Ctrl+D (0x04) instead".to_owned());
        }
        self.stdin.lock().await.take();
        Ok(())
    }

    pub async fn resize(&self, rows: u32, cols: u32) -> Result<(), String> {
        let pty = self.pty.as_ref().ok_or("tty not assigned to process")?;
        pty.lock()
            .await
            .resize(Size::new(rows as u16, cols as u16))
            .map_err(|e| e.to_string())
    }

    pub fn signal(&self, signal: Signal) -> Result<(), String> {
        if matches!(signal, Signal::SIGKILL | Signal::SIGTERM) {
            self.stop_output.cancel();
        }
        if self.end.borrow().is_some() {
            return Err("os: process already finished".to_owned());
        }
        kill(Pid::from_raw(self.pid as i32), signal).map_err(|errno| reason(&io::Error::from(errno)))
    }
}

pub struct Launch {
    pub request: StartRequest,
    pub placement: Option<Placement>,
    pub user: User,
    pub workdir: Option<String>,
    pub global_env: BTreeMap<String, String>,
    pub deadline: Option<Instant>,
}

static NICE_PREFIX: LazyLock<String> = LazyLock::new(nice_prefix);

struct OutputPipe {
    reader: Box<dyn AsyncRead + Send + Unpin>,
    chunk_size: usize,
    kind: fn(Vec<u8>) -> Data,
}

struct Started {
    child: Child,
    stdin: Option<ChildStdin>,
    pty_writer: Option<OwnedWritePty>,
    pipes: Vec<OutputPipe>,
}

pub fn spawn(launch: Launch, processes: Arc<Processes>) -> Result<(Arc<Process>, Receiver<DataEvent>), ConnectError> {
    let Launch {
        mut request,
        placement,
        user,
        workdir,
        global_env,
        deadline,
    } = launch;
    let config = request.process.take().unwrap_or_default();
    let command_line: Vec<String> = std::iter::once(config.cmd.clone())
        .chain(config.args.iter().cloned())
        .collect();
    let user_command = command_line.join(" ");

    let cwd = expand_and_resolve(config.cwd.as_deref().unwrap_or_default(), &user, workdir.as_deref())
        .map_err(ConnectError::invalid_argument)?;
    if std::fs::metadata(&cwd).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        return Err(ConnectError::invalid_argument(format!(
            "cwd '{}' does not exist",
            cwd.display()
        )));
    }

    let env = environment(
        &std::env::var("PATH").unwrap_or_default(),
        &user,
        &global_env,
        &config.envs,
    );
    let args: Vec<String> = ["-c".to_owned(), wrapper_script(&NICE_PREFIX), "--".to_owned()]
        .into_iter()
        .chain(command_line)
        .collect();

    let expired = deadline.is_some_and(|deadline| Instant::now() >= deadline);
    let started = match request.pty.as_option() {
        Some(pty) => unexpired(expired)
            .and_then(|()| {
                start_pty(
                    &args,
                    &env,
                    &cwd,
                    &user,
                    placement.as_ref(),
                    pty.size.rows,
                    pty.size.cols,
                )
            })
            .map_err(|e| {
                start_error(
                    &e,
                    format!(
                        "error starting pty with command '{user_command}' in dir '{}' with '{}' cols and '{}' rows: {}",
                        cwd.display(),
                        pty.size.cols,
                        pty.size.rows,
                        reason(&e)
                    ),
                )
            })?,
        None => unexpired(expired)
            .and_then(|()| {
                start_pipes(
                    &args,
                    &env,
                    &cwd,
                    &user,
                    placement.as_ref(),
                    request.stdin.unwrap_or(true),
                )
            })
            .map_err(|e| {
                let cause = match e.kind() {
                    io::ErrorKind::TimedOut => reason(&e),
                    _ => format!("fork/exec /bin/sh: {}", reason(&e)),
                };
                start_error(&e, format!("error starting process '{user_command}': {cause}"))
            })?,
    };
    let Started {
        child,
        stdin,
        pty_writer,
        pipes,
    } = started;

    let process = Arc::new(Process {
        pid: child.id().unwrap_or_default(),
        tag: request.tag.take(),
        config,
        output: Output::new(),
        end: watch::Sender::new(None),
        stdin: Mutex::new(stdin),
        pty: pty_writer.map(Mutex::new),
        stop_output: CancellationToken::new(),
    });
    let subscription = process.output.subscribe();
    processes.add(process.clone());
    tokio::spawn(run(process.clone(), child, pipes, deadline, processes));

    Ok((process, subscription))
}

fn start_pipes(
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    user: &User,
    placement: Option<&Placement>,
    stdin: bool,
) -> io::Result<Started> {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(args)
        .env_clear()
        .envs(env.iter().cloned())
        .current_dir(cwd)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(placement) = placement {
        let procs = placement.raw_fd();
        unsafe { command.pre_exec(move || Placement::join(procs)) };
    }

    let mut child = spawn_as(&mut command, user)?;
    let stdout = child.stdout.take().map(|stdout| OutputPipe {
        reader: Box::new(stdout),
        chunk_size: PIPE_CHUNK,
        kind: Data::Stdout,
    });
    let stderr = child.stderr.take().map(|stderr| OutputPipe {
        reader: Box::new(stderr),
        chunk_size: PIPE_CHUNK,
        kind: Data::Stderr,
    });
    Ok(Started {
        stdin: child.stdin.take(),
        pty_writer: None,
        pipes: stdout.into_iter().chain(stderr).collect(),
        child,
    })
}

fn start_pty(
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    user: &User,
    placement: Option<&Placement>,
    rows: u32,
    cols: u32,
) -> io::Result<Started> {
    let (pty, pts) = pty_process::open().map_err(io::Error::other)?;
    pty.resize(Size::new(rows as u16, cols as u16))
        .map_err(io::Error::other)?;

    let command = pty_process::Command::new("/bin/sh")
        .args(args)
        .env_clear()
        .envs(env.iter().cloned())
        .current_dir(cwd);
    let command = unsafe { command.pre_exec(switch_user(user)) };
    let command = match placement.map(Placement::raw_fd) {
        Some(procs) => unsafe { command.pre_exec(move || Placement::join(procs)) },
        None => command,
    };
    let child = command.spawn(pts).map_err(io::Error::other)?;

    let (reader, writer) = pty.into_split();
    Ok(Started {
        child,
        stdin: None,
        pty_writer: Some(writer),
        pipes: vec![OutputPipe {
            reader: Box::new(reader),
            chunk_size: PTY_CHUNK,
            kind: Data::Pty,
        }],
    })
}

#[cfg(target_os = "linux")]
fn spawn_as(command: &mut tokio::process::Command, user: &User) -> io::Result<Child> {
    super::credentials::with_user_credentials(user, || command.spawn())
}

#[cfg(not(target_os = "linux"))]
fn spawn_as(command: &mut tokio::process::Command, user: &User) -> io::Result<Child> {
    unsafe { command.pre_exec(switch_user(user)) }.spawn()
}

fn switch_user(user: &User) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    #[cfg(target_os = "linux")]
    let groups: Vec<Gid> = std::iter::once(user.gid)
        .chain(user.groups.iter().copied())
        .map(Gid::from_raw)
        .collect();
    let (uid, gid) = (Uid::from_raw(user.uid), Gid::from_raw(user.gid));
    move || {
        #[cfg(target_os = "linux")]
        nix::unistd::setgroups(&groups)?;
        nix::unistd::setgid(gid)?;
        nix::unistd::setuid(uid)?;
        Ok(())
    }
}

async fn pump(process: Arc<Process>, mut pipe: OutputPipe) {
    let mut buffer = vec![0; pipe.chunk_size];
    loop {
        let read = tokio::select! {
            _ = process.stop_output.cancelled() => return,
            read = pipe.reader.read(&mut buffer) => read,
        };
        match read {
            Ok(0) => return,
            Ok(n) if process.output.has_subscribers() => {
                let event = DataEvent {
                    output: Some((pipe.kind)(buffer[..n].to_vec())),
                    ..Default::default()
                };
                process.output.publish(event).await;
            }
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(Errno::EIO as i32) => return,
            Err(e) => {
                tracing::error!(pid = process.pid, error = %e, "error reading process output");
                return;
            }
        }
    }
}

async fn run(
    process: Arc<Process>,
    mut child: Child,
    pipes: Vec<OutputPipe>,
    deadline: Option<Instant>,
    processes: Arc<Processes>,
) {
    let pid = Pid::from_raw(process.pid as i32);
    let mut readers: JoinSet<()> = pipes.into_iter().map(|pipe| pump(process.clone(), pipe)).collect();
    tokio::select! {
        _ = async { while readers.join_next().await.is_some() {} } => {}
        _ = process.stop_output.cancelled() => {}
        _ = until(deadline) => { let _ = kill(pid, Signal::SIGKILL); }
    }
    readers.abort_all();
    process.output.close();

    let status = tokio::select! {
        status = child.wait() => status,
        _ = until(deadline) => {
            let _ = kill(pid, Signal::SIGKILL);
            child.wait().await
        }
    };
    let end_event = end_event(status);
    process.end.send_replace(Some(end_event.clone()));
    processes.finish(&process, end_event);
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

pub fn environment<'a>(
    path: &str,
    user: &User,
    global: &BTreeMap<String, String>,
    request: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<(String, String)> {
    let home = user.home.display().to_string();
    [
        ("PATH", path),
        ("HOME", home.as_str()),
        ("USER", &user.name),
        ("LOGNAME", &user.name),
    ]
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .into_iter()
    .chain(global.iter().map(|(key, value)| (key.clone(), value.clone())))
    .chain(request.into_iter().map(|(key, value)| (key.clone(), value.clone())))
    .collect()
}

pub fn wrapper_script(prefix: &str) -> String {
    format!("echo {OOM_SCORE} > /proc/$$/oom_score_adj && exec {prefix}\"${{@}}\"")
}

fn nice_prefix() -> String {
    let nice_delta = -rustix::process::getpriority_process(None).unwrap_or(0);
    let ionice = look_path("ionice").map(|path| format!("{} -c {IO_CLASS} -n {IO_PRIORITY} ", path.display()));
    let nice = look_path("nice").map(|path| format!("{} -n {nice_delta} ", path.display()));
    ionice.into_iter().chain(nice).collect()
}

fn look_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    })
}

fn unexpired(expired: bool) -> io::Result<()> {
    match expired {
        true => Err(io::Error::new(io::ErrorKind::TimedOut, "context deadline exceeded")),
        false => Ok(()),
    }
}

fn start_error(err: &io::Error, message: String) -> ConnectError {
    let exhausted = [
        Errno::EAGAIN,
        Errno::ENOMEM,
        Errno::EMFILE,
        Errno::ENFILE,
        Errno::ENOSPC,
    ]
    .iter()
    .any(|errno| err.raw_os_error() == Some(*errno as i32));
    let code = match (exhausted, err.kind()) {
        (true, _) => ErrorCode::ResourceExhausted,
        (false, io::ErrorKind::TimedOut) => ErrorCode::DeadlineExceeded,
        (false, _) => ErrorCode::InvalidArgument,
    };
    ConnectError::new(code, message)
}

pub fn end_event(status: io::Result<ExitStatus>) -> EndEvent {
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            return EndEvent {
                exit_code: -1,
                error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };
    let text = exit_text(&status);
    EndEvent {
        exit_code: status.code().unwrap_or(-1),
        exited: status.code().is_some(),
        error: (!status.success()).then(|| text.clone()),
        status: text,
        ..Default::default()
    }
}

fn exit_text(status: &ExitStatus) -> String {
    let base = match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("signal: {}", signal_name(signal)),
        (None, None) => String::new(),
    };
    if status.core_dumped() {
        return format!("{base} (core dumped)");
    }
    base
}

fn signal_name(signal: i32) -> String {
    const NAMES: [&str; 31] = [
        "hangup",
        "interrupt",
        "quit",
        "illegal instruction",
        "trace/breakpoint trap",
        "aborted",
        "bus error",
        "floating point exception",
        "killed",
        "user defined signal 1",
        "segmentation fault",
        "user defined signal 2",
        "broken pipe",
        "alarm clock",
        "terminated",
        "stack fault",
        "child exited",
        "continued",
        "stopped (signal)",
        "stopped",
        "stopped (tty input)",
        "stopped (tty output)",
        "urgent I/O condition",
        "CPU time limit exceeded",
        "file size limit exceeded",
        "virtual timer expired",
        "profiling timer expired",
        "window changed",
        "I/O possible",
        "power failure",
        "bad system call",
    ];
    usize::try_from(signal - 1)
        .ok()
        .and_then(|index| NAMES.get(index))
        .map_or_else(|| format!("signal {signal}"), |name| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    #[test]
    fn environment_layers_base_global_then_request() {
        let user = User {
            name: "user".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/user".into(),
            groups: vec![],
        };
        let global = BTreeMap::from([pair("A", "global")]);
        let request = HashMap::from([pair("A", "request")]);

        assert_eq!(
            environment("/usr/bin", &user, &global, &request),
            vec![
                pair("PATH", "/usr/bin"),
                pair("HOME", "/home/user"),
                pair("USER", "user"),
                pair("LOGNAME", "user"),
                pair("A", "global"),
                pair("A", "request"),
            ]
        );
    }

    #[test]
    fn wrapper_matches_go() {
        assert_eq!(
            wrapper_script("/usr/bin/ionice -c 2 -n 4 /usr/bin/nice -n 0 "),
            "echo 100 > /proc/$$/oom_score_adj && exec /usr/bin/ionice -c 2 -n 4 /usr/bin/nice -n 0 \"${@}\""
        );
        assert_eq!(wrapper_script(""), "echo 100 > /proc/$$/oom_score_adj && exec \"${@}\"");
    }

    #[test]
    fn end_events_like_go_process_state() {
        let exited = |code: i32| end_event(Ok(ExitStatus::from_raw(code << 8)));
        let signaled = |signal: i32| end_event(Ok(ExitStatus::from_raw(signal)));

        let ok = exited(0);
        assert_eq!(
            (ok.exit_code, ok.exited, ok.status.as_str(), ok.error),
            (0, true, "exit status 0", None)
        );

        let failed = exited(3);
        assert_eq!((failed.exit_code, failed.status.as_str()), (3, "exit status 3"));
        assert_eq!(failed.error.as_deref(), Some("exit status 3"));

        let killed = signaled(9);
        assert_eq!((killed.exit_code, killed.exited), (-1, false));
        assert_eq!(killed.status, "signal: killed");
        assert_eq!(signaled(15).status, "signal: terminated");
        assert_eq!(signaled(11 | 0x80).status, "signal: segmentation fault (core dumped)");
        assert_eq!(signal_name(64), "signal 64");
    }
}
