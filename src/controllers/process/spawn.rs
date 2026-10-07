use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Instant;

use connectrpc::{ConnectError, ErrorCode};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Gid, Pid, Uid};
use pty_process::{OwnedWritePty, Size};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc::Receiver;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::output::Output;
use super::registry::Processes;
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
    pub end: watch::Receiver<Option<EndEvent>>,
    pub stdin: Mutex<Option<ChildStdin>>,
    pub pty: Option<Mutex<OwnedWritePty>>,
    pub stop_output: CancellationToken,
}

pub struct Launch {
    pub request: StartRequest,
    pub user: User,
    pub workdir: Option<String>,
    pub global_env: BTreeMap<String, String>,
    pub deadline: Option<Instant>,
}

pub fn spawn(launch: Launch, processes: Arc<Processes>) -> Result<(Arc<Process>, Receiver<DataEvent>), ConnectError> {
    let config = launch.request.process.clone().into_option().unwrap_or_default();
    let user_command = std::iter::once(config.cmd.as_str())
        .chain(config.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");

    let cwd = expand_and_resolve(
        config.cwd.as_deref().unwrap_or_default(),
        &launch.user,
        launch.workdir.as_deref(),
    )
    .map_err(ConnectError::invalid_argument)?;
    if std::fs::metadata(&cwd).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        return Err(ConnectError::invalid_argument(format!(
            "cwd '{}' does not exist",
            cwd.display()
        )));
    }

    let path = std::env::var("PATH").unwrap_or_default();
    let env = environment(&path, &launch.user, &launch.global_env, &config.envs);
    let args: Vec<String> = ["-c".to_owned(), wrapper_script(&nice_prefix()), "--".to_owned()]
        .into_iter()
        .chain(std::iter::once(config.cmd.clone()))
        .chain(config.args.iter().cloned())
        .collect();

    let (end_sender, end) = watch::channel(None);
    let started = match launch.request.pty.as_option() {
        Some(pty) => start_pty(&args, &env, &cwd, &launch.user, pty.size.rows, pty.size.cols).map_err(|e| {
            let size = &pty.size;
            start_error(
                &e,
                format!(
                    "error starting pty with command '{user_command}' in dir '{}' with '{}' cols and '{}' rows: {}",
                    cwd.display(),
                    size.cols,
                    size.rows,
                    reason(&e)
                ),
            )
        })?,
        None => start_pipes(&args, &env, &cwd, &launch.user, launch.request.stdin.unwrap_or(true)).map_err(|e| {
            start_error(
                &e,
                format!(
                    "error starting process '{user_command}': fork/exec /bin/sh: {}",
                    reason(&e)
                ),
            )
        })?,
    };

    let process = Arc::new(Process {
        pid: started.child.id().unwrap_or_default(),
        tag: launch.request.tag.clone(),
        config,
        output: Output::new(),
        end,
        stdin: Mutex::new(started.stdin),
        pty: started.pty_writer.map(Mutex::new),
        stop_output: CancellationToken::new(),
    });
    let subscription = process.output.subscribe();
    processes.add(process.clone());

    let mut readers = JoinSet::new();
    for (reader, chunk, kind) in started.readers {
        readers.spawn(pump(process.clone(), reader, chunk, kind));
    }
    tokio::spawn(wait(
        process.clone(),
        started.child,
        readers,
        end_sender,
        launch.deadline,
        processes,
    ));

    Ok((process, subscription))
}

type Reader = (Box<dyn AsyncRead + Send + Unpin>, usize, fn(Vec<u8>) -> Data);

struct Started {
    child: Child,
    stdin: Option<ChildStdin>,
    pty_writer: Option<OwnedWritePty>,
    readers: Vec<Reader>,
}

fn start_pipes(args: &[String], env: &[(String, String)], cwd: &Path, user: &User, stdin: bool) -> io::Result<Started> {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(args)
        .env_clear()
        .envs(env.iter().cloned())
        .current_dir(cwd)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(switch_user(user));
    }

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .map(|out| (boxed(out), PIPE_CHUNK, Data::Stdout as fn(Vec<u8>) -> Data));
    let stderr = child
        .stderr
        .take()
        .map(|err| (boxed(err), PIPE_CHUNK, Data::Stderr as fn(Vec<u8>) -> Data));
    Ok(Started {
        stdin: child.stdin.take(),
        pty_writer: None,
        readers: stdout.into_iter().chain(stderr).collect(),
        child,
    })
}

fn start_pty(
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    user: &User,
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
    let child = command.spawn(pts).map_err(io::Error::other)?;

    let (reader, writer) = pty.into_split();
    Ok(Started {
        child,
        stdin: None,
        pty_writer: Some(writer),
        readers: vec![(boxed(reader), PTY_CHUNK, Data::Pty as fn(Vec<u8>) -> Data)],
    })
}

fn boxed(reader: impl AsyncRead + Send + Unpin + 'static) -> Box<dyn AsyncRead + Send + Unpin> {
    Box::new(reader)
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

async fn pump(
    process: Arc<Process>,
    mut reader: Box<dyn AsyncRead + Send + Unpin>,
    chunk: usize,
    kind: fn(Vec<u8>) -> Data,
) {
    let mut buffer = vec![0; chunk];
    loop {
        let read = tokio::select! {
            _ = process.stop_output.cancelled() => return,
            read = reader.read(&mut buffer) => read,
        };
        match read {
            Ok(0) => return,
            Ok(n) if process.output.has_subscribers() => {
                let event = DataEvent {
                    output: Some(kind(buffer[..n].to_vec())),
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

async fn wait(
    process: Arc<Process>,
    mut child: Child,
    mut readers: JoinSet<()>,
    end: watch::Sender<Option<EndEvent>>,
    deadline: Option<Instant>,
    processes: Arc<Processes>,
) {
    let pid = Pid::from_raw(process.pid as i32);
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
    end.send_replace(Some(end_event.clone()));
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
    let code = if exhausted {
        ErrorCode::ResourceExhausted
    } else {
        ErrorCode::InvalidArgument
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
