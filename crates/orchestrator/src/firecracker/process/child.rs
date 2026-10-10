mod clone3;

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use nix::fcntl::OFlag;
use nix::libc;
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use tokio::io::unix::AsyncFd;
use tokio::net::unix::pipe;

use self::clone3::{Forked, fork_into};

pub struct Child {
    pid: u32,
    pidfd: AsyncFd<OwnedFd>,
    status: Option<ExitStatus>,
}

pub struct Spawned {
    pub child: Child,
    pub stdout: pipe::Receiver,
    pub stderr: pipe::Receiver,
}

struct Exec {
    program: CString,
    _strings: Vec<CString>,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
}

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

pub fn spawn(program: &Path, args: &[&str], cgroup: Option<&Path>) -> io::Result<Spawned> {
    let exec = Exec::new(program, args)?;
    let cgroup = cgroup.map(open_directory).transpose()?;
    let stdin = File::open("/dev/null")?;
    let stdout = Pipe::new()?;
    let stderr = Pipe::new()?;
    match unsafe { fork_into(cgroup.as_ref().map(AsFd::as_fd)) }? {
        Forked::Child => exec.replace_child([stdin.as_raw_fd(), stdout.write.as_raw_fd(), stderr.write.as_raw_fd()]),
        Forked::Parent { pid, pidfd } => Ok(Spawned {
            child: Child {
                pid,
                pidfd: AsyncFd::new(pidfd)?,
                status: None,
            },
            stdout: pipe::Receiver::from_owned_fd(stdout.read)?,
            stderr: pipe::Receiver::from_owned_fd(stderr.read)?,
        }),
    }
}

impl Child {
    pub fn id(&self) -> Option<u32> {
        self.status.is_none().then_some(self.pid)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = reap(self.pidfd.get_ref(), WaitPidFlag::WNOHANG)?;
        }
        Ok(self.status)
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            self.pidfd.readable().await?.clear_ready();
        }
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_none() {
            send_kill(self.pidfd.get_ref())?;
        }
        self.wait().await.map(|_| ())
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if !matches!(self.try_wait(), Ok(None)) {
            return;
        }
        let _ = send_kill(self.pidfd.get_ref());
        let pidfd = self.pidfd.get_ref().try_clone();
        std::thread::spawn(move || pidfd.map(|pidfd| reap(&pidfd, WaitPidFlag::empty())));
    }
}

impl Exec {
    fn new(program: &Path, args: &[&str]) -> io::Result<Self> {
        let program = c_string(program.as_os_str())?;
        let args = std::iter::once(Ok(program.clone()))
            .chain(args.iter().map(|arg| c_string(OsStr::new(arg))))
            .collect::<io::Result<Vec<_>>>()?;
        let vars = std::env::vars_os()
            .map(|(name, value)| c_string(OsStr::from_bytes(&[name.as_bytes(), b"=", value.as_bytes()].concat())))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            program,
            argv: null_terminated(&args),
            envp: null_terminated(&vars),
            _strings: args.into_iter().chain(vars).collect(),
        })
    }

    fn replace_child(&self, stdio: [RawFd; 3]) -> ! {
        unsafe {
            libc::setsid();
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            let mut unblocked: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut unblocked);
            libc::sigprocmask(libc::SIG_SETMASK, &unblocked, std::ptr::null_mut());
            for (target, source) in stdio.into_iter().enumerate() {
                libc::dup2(source, target as RawFd);
            }
            libc::execve(self.program.as_ptr(), self.argv.as_ptr(), self.envp.as_ptr());
            libc::_exit(127)
        }
    }
}

impl Pipe {
    fn new() -> io::Result<Self> {
        let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        Ok(Self { read, write })
    }
}

fn reap(pidfd: &OwnedFd, flags: WaitPidFlag) -> io::Result<Option<ExitStatus>> {
    match waitid(Id::PIDFd(pidfd.as_fd()), WaitPidFlag::WEXITED | flags)? {
        WaitStatus::Exited(_, code) => Ok(Some(ExitStatus::from_raw(code << 8))),
        WaitStatus::Signaled(_, signal, core) => {
            Ok(Some(ExitStatus::from_raw(signal as i32 | if core { 0x80 } else { 0 })))
        }
        _ => Ok(None),
    }
}

fn send_kill(pidfd: &OwnedFd) -> io::Result<()> {
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    match sent {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("open cgroup {}: {e}", path.display())))
}

fn c_string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn null_terminated(strings: &[CString]) -> Vec<*const libc::c_char> {
    strings
        .iter()
        .map(|string| string.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

pub fn find_in_path(program: &str) -> io::Result<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{program} is not on PATH")))
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    async fn output(spawned: Spawned) -> (String, ExitStatus) {
        let Spawned {
            mut child, mut stdout, ..
        } = spawned;
        let mut text = String::new();
        stdout.read_to_string(&mut text).await.unwrap();
        (text, child.wait().await.unwrap())
    }

    fn shell(script: &str, cgroup: Option<&Path>) -> Spawned {
        spawn(Path::new("/bin/sh"), &["-c", script], cgroup).unwrap()
    }

    #[tokio::test]
    async fn captures_stdout_and_the_exit_code() {
        let (text, status) = output(shell("echo hello; exit 3", None)).await;

        assert_eq!((text.as_str(), status.code()), ("hello\n", Some(3)));
    }

    #[tokio::test]
    async fn kill_ends_the_process_and_forgets_its_pid() {
        let mut spawned = shell("sleep 30", None);

        spawned.child.kill().await.unwrap();

        assert_eq!(spawned.child.try_wait().unwrap().unwrap().signal(), Some(libc::SIGKILL));
        assert_eq!(spawned.child.id(), None);
    }

    #[tokio::test]
    async fn the_child_leads_a_new_session() {
        let spawned = shell("exec cat /proc/self/stat", None);
        let pid = spawned.child.id().unwrap();

        let (stat, _) = output(spawned).await;
        let session = stat.rsplit(')').next().unwrap().split_whitespace().nth(3).unwrap();

        assert_eq!(session, pid.to_string());
    }

    #[tokio::test]
    async fn starts_inside_the_cgroup_without_moving() {
        if !nix::unistd::geteuid().is_root() {
            return;
        }
        let name = format!("child-test-{}", std::process::id());
        let cgroup = Path::new("/sys/fs/cgroup").join(&name);
        std::fs::create_dir(&cgroup).unwrap();

        let (membership, _) = output(shell("cat /proc/self/cgroup", Some(&cgroup))).await;

        std::fs::remove_dir(&cgroup).unwrap();
        assert_eq!(membership.trim(), format!("0::/{name}"));
    }

    #[test]
    fn finds_programs_on_the_path() {
        assert!(find_in_path("sh").unwrap().is_file());
        assert_eq!(
            find_in_path("no-such-program-here").unwrap_err().to_string(),
            "no-such-program-here is not on PATH"
        );
    }
}
