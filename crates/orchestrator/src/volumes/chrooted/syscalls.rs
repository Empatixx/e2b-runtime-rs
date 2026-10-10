use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
use rustix::io::Errno;

use super::metrics::count_resolve_retries_exhausted;

const MAX_RESOLVE_RETRIES: usize = 32;
const MODE_SETUID: u32 = 1 << 23;
const MODE_SETGID: u32 = 1 << 22;
const MODE_STICKY: u32 = 1 << 20;
const PERMISSION_BITS: u32 = 0o777;
const SYS_FCHMODAT2: nix::libc::c_long = 452;

pub fn retry_interrupted<T>(mut call: impl FnMut() -> Result<T, Errno>) -> Result<T, Errno> {
    loop {
        match call() {
            Err(Errno::INTR) => continue,
            result => return result,
        }
    }
}

pub fn resolve_open(root: BorrowedFd<'_>, name: &str, flags: OFlags, mode: Mode) -> Result<OwnedFd, Errno> {
    let mode = if flags.contains(OFlags::CREATE) {
        mode
    } else {
        Mode::empty()
    };
    (0..MAX_RESOLVE_RETRIES)
        .map(|attempt| {
            if attempt > 0 {
                std::thread::yield_now();
            }
            retry_interrupted(|| {
                openat2(
                    root,
                    name,
                    flags | OFlags::CLOEXEC,
                    mode,
                    ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
                )
            })
        })
        .find(|opened| !matches!(opened, Err(Errno::AGAIN)))
        .unwrap_or_else(|| {
            count_resolve_retries_exhausted();
            Err(Errno::AGAIN)
        })
}

pub fn syscall_mode(file_mode: u32) -> Mode {
    [
        (MODE_SETUID, Mode::SUID),
        (MODE_SETGID, Mode::SGID),
        (MODE_STICKY, Mode::SVTX),
    ]
    .into_iter()
    .filter(|(bit, _)| file_mode & bit != 0)
    .fold(
        Mode::from_bits_truncate(file_mode & PERMISSION_BITS),
        |mode, (_, special)| mode | special,
    )
}

pub fn fchmodat2_empty_path(fd: BorrowedFd<'_>, mode: Mode) -> Result<(), Errno> {
    let result = unsafe {
        nix::libc::syscall(
            SYS_FCHMODAT2,
            fd.as_raw_fd(),
            c"".as_ptr(),
            mode.bits(),
            nix::libc::AT_EMPTY_PATH,
        )
    };
    match result {
        0 => Ok(()),
        _ => Err(Errno::from_raw_os_error(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(nix::libc::EIO),
        )),
    }
}

pub fn procfs_path(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_file_mode_bits_become_syscall_mode_bits() {
        assert_eq!(syscall_mode(0o644), Mode::from_bits_truncate(0o644));
        assert_eq!(syscall_mode(0o4755), Mode::from_bits_truncate(0o755));
        assert_eq!(
            syscall_mode(MODE_SETUID | MODE_STICKY | 0o700),
            Mode::from_bits_truncate(0o5700)
        );
        assert_eq!(syscall_mode(MODE_SETGID), Mode::SGID);
    }

    #[test]
    fn interrupted_calls_are_repeated() {
        let mut calls = 0;

        let result = retry_interrupted(|| {
            calls += 1;
            if calls < 3 { Err(Errno::INTR) } else { Ok(calls) }
        });

        assert_eq!(result, Ok(3));
    }
}
