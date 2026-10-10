use std::fmt;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use futures::future::BoxFuture;
use nix::libc;

use super::scope::Scope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    NotExecutable(String),
    Signalled(i32),
    Exited(i32),
    Other(String),
}

impl ProbeFailure {
    pub fn from_spawn(path: &Path, error: &io::Error) -> Self {
        let text = format!("fork/exec {}: {}", path.display(), crate::go_os::reason(error));
        match error.raw_os_error() {
            Some(libc::ENOEXEC) => ProbeFailure::NotExecutable(text),
            _ => ProbeFailure::Other(text),
        }
    }

    pub fn from_status(status: ExitStatus) -> Self {
        match (status.code(), status.signal()) {
            (Some(code), _) => ProbeFailure::Exited(code),
            (None, Some(signal)) => ProbeFailure::Signalled(signal),
            (None, None) => ProbeFailure::Other("unknown exit".to_owned()),
        }
    }

    pub fn about_the_bytes(&self, scope: &Scope) -> bool {
        if scope.error().is_some() {
            return false;
        }
        match self {
            ProbeFailure::NotExecutable(_) => true,
            ProbeFailure::Signalled(signal) => matches!(*signal, libc::SIGBUS | libc::SIGSEGV | libc::SIGILL),
            ProbeFailure::Exited(_) | ProbeFailure::Other(_) => false,
        }
    }
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeFailure::NotExecutable(text) | ProbeFailure::Other(text) => f.write_str(text),
            ProbeFailure::Exited(code) => write!(f, "exit status {code}"),
            ProbeFailure::Signalled(signal) => write!(f, "signal: {}", signal_text(*signal)),
        }
    }
}

fn signal_text(signal: i32) -> String {
    match signal {
        libc::SIGBUS => "bus error".to_owned(),
        libc::SIGSEGV => "segmentation fault".to_owned(),
        libc::SIGILL => "illegal instruction".to_owned(),
        libc::SIGKILL => "killed".to_owned(),
        libc::SIGTERM => "terminated".to_owned(),
        other => format!("signal {other}"),
    }
}

pub trait Probe: Send + Sync {
    fn version<'a>(&'a self, scope: &'a Scope, path: &'a Path) -> BoxFuture<'a, Result<String, ProbeFailure>>;
}

type ProbeFn = dyn Fn(Scope, PathBuf) -> BoxFuture<'static, Result<String, ProbeFailure>> + Send + Sync;

pub struct FnProbe(Arc<ProbeFn>);

impl FnProbe {
    pub fn shared(
        probe: impl Fn(Scope, PathBuf) -> BoxFuture<'static, Result<String, ProbeFailure>> + Send + Sync + 'static,
    ) -> Arc<dyn Probe> {
        Arc::new(Self(Arc::new(probe)))
    }
}

impl Probe for FnProbe {
    fn version<'a>(&'a self, scope: &'a Scope, path: &'a Path) -> BoxFuture<'a, Result<String, ProbeFailure>> {
        (self.0)(scope.clone(), path.to_path_buf())
    }
}

pub struct ExecProbe;

impl Probe for ExecProbe {
    fn version<'a>(&'a self, scope: &'a Scope, path: &'a Path) -> BoxFuture<'a, Result<String, ProbeFailure>> {
        Box::pin(async move { run_version(scope, path).await })
    }
}

async fn run_version(scope: &Scope, path: &Path) -> Result<String, ProbeFailure> {
    let child = tokio::process::Command::new(path)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ProbeFailure::from_spawn(path, &e))?;
    let output = tokio::select! {
        output = child.wait_with_output() => output.map_err(|e| ProbeFailure::Other(e.to_string()))?,
        _ = scope.done() => return Err(ProbeFailure::Signalled(libc::SIGKILL)),
    };
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        false => Err(ProbeFailure::from_status(output.status)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;

    fn status_of(script: &str) -> ExitStatus {
        Command::new("/bin/sh").args(["-c", script]).status().unwrap()
    }

    fn io_error(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    fn judged(failure: &ProbeFailure) -> bool {
        failure.about_the_bytes(&Scope::background())
    }

    #[test]
    fn only_a_verdict_about_the_bytes_pins_an_identity() {
        let path = Path::new("/cache/envd");
        for (name, failure, pins) in [
            (
                "the loader refusing the image",
                ProbeFailure::from_spawn(path, &io_error(libc::ENOEXEC)),
                true,
            ),
            (
                "a child that ran and exited nonzero",
                ProbeFailure::from_status(status_of("exit 3")),
                false,
            ),
            (
                "a copy that vanished under the probe",
                ProbeFailure::from_spawn(path, &io_error(libc::ENOENT)),
                false,
            ),
            (
                "fd exhaustion allocating the probe's pipes",
                ProbeFailure::from_spawn(path, &io_error(libc::EMFILE)),
                false,
            ),
            (
                "a packed node failing fork",
                ProbeFailure::from_spawn(path, &io_error(libc::ENOMEM)),
                false,
            ),
            (
                "a cache directory mounted noexec",
                ProbeFailure::from_spawn(path, &io_error(libc::EACCES)),
                false,
            ),
            (
                "an image that faults on its own instructions",
                ProbeFailure::from_status(status_of("kill -SEGV $$")),
                true,
            ),
            (
                "an image killed by SIGBUS",
                ProbeFailure::from_status(status_of("kill -BUS $$")),
                true,
            ),
            (
                "an image killed by SIGILL",
                ProbeFailure::from_status(status_of("kill -ILL $$")),
                true,
            ),
            (
                "a child killed by an unrelated signal",
                ProbeFailure::from_status(status_of("kill -TERM $$")),
                false,
            ),
        ] {
            assert_eq!(judged(&failure), pins, "{name}: {failure}");
        }
    }

    #[tokio::test]
    async fn a_probe_stopped_by_the_warms_own_deadline_is_not_a_verdict() {
        let (scope, cancel) = Scope::cancellable();
        let failure = ProbeFailure::from_status(status_of("kill -SEGV $$"));

        cancel.cancel();

        assert!(!failure.about_the_bytes(&scope));
    }

    #[test]
    fn the_failures_read_like_go() {
        assert_eq!(ProbeFailure::Exited(3).to_string(), "exit status 3");
        assert_eq!(ProbeFailure::Signalled(libc::SIGKILL).to_string(), "signal: killed");
        assert_eq!(
            ProbeFailure::Signalled(libc::SIGSEGV).to_string(),
            "signal: segmentation fault"
        );
    }

    #[tokio::test]
    async fn the_exec_probe_reads_the_version_the_binary_prints() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("envd");
        std::fs::write(&binary, "#!/bin/sh\necho ' 0.9.1 '\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let version = ExecProbe.version(&Scope::background(), &binary).await;

        assert_eq!(version, Ok("0.9.1".to_owned()));
    }

    #[tokio::test]
    async fn the_exec_probe_reports_a_nonzero_exit() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("envd");
        std::fs::write(&binary, "#!/bin/sh\nexit 2\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let version = ExecProbe.version(&Scope::background(), &binary).await;

        assert_eq!(version, Err(ProbeFailure::Exited(2)));
    }
}
