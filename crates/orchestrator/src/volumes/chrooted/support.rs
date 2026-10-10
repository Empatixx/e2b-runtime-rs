use std::sync::OnceLock;

use rustix::fs::{Mode, OFlags, ResolveFlags, open, openat2};
use rustix::io::Errno;

use super::error::PathError;
use super::syscalls::retry_interrupted;
use crate::go_os::reason;

static SUPPORT: OnceLock<Result<(), String>> = OnceLock::new();

pub fn check_support() -> Result<(), String> {
    SUPPORT.get_or_init(probe).clone()
}

fn probe() -> Result<(), String> {
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let root = retry_interrupted(|| open("/", flags, Mode::empty())).map_err(|errno| {
        PathError::builder()
            .operation("open")
            .path("/")
            .errno(errno)
            .build()
            .to_string()
    })?;
    openat2(
        &root,
        "/",
        flags,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )
    .map(drop)
    .map_err(unsupported)
}

fn unsupported(errno: Errno) -> String {
    format!(
        "kernel does not support openat2 with RESOLVE_IN_ROOT: {}",
        reason(&std::io::Error::from_raw_os_error(errno.raw_os_error()))
    )
}
