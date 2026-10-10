use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use nix::libc;

const CLONE_PIDFD: u64 = 0x1000;
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

pub enum Forked {
    Child,
    Parent { pid: u32, pidfd: OwnedFd },
}

#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

pub unsafe fn fork_into(cgroup: Option<BorrowedFd>) -> io::Result<Forked> {
    let mut pidfd: libc::c_int = -1;
    let args = CloneArgs {
        flags: CLONE_PIDFD | cgroup.map_or(0, |_| CLONE_INTO_CGROUP),
        pidfd: &mut pidfd as *mut libc::c_int as u64,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: cgroup.map_or(0, |fd| fd.as_raw_fd() as u64),
        ..CloneArgs::default()
    };
    match unsafe { libc::syscall(libc::SYS_clone3, &args, size_of::<CloneArgs>()) } {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        pid => Ok(Forked::Parent {
            pid: pid as u32,
            pidfd: unsafe { OwnedFd::from_raw_fd(pidfd) },
        }),
    }
}
