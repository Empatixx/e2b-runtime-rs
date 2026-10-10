use std::ffi::CStr;
use std::os::fd::{AsFd, BorrowedFd};

use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};
use rustix::io::Errno;

use super::directory::entry_names;
use super::names::check_component;
use super::syscalls::retry_interrupted;

pub const REMOVE_ALL_BATCH: usize = 1024;

enum Drained {
    NotADirectory,
    Gone,
    Empty,
}

pub fn remove_all_at(directory: BorrowedFd<'_>, name: &CStr) -> Result<(), Errno> {
    check_component(name.to_bytes())?;
    let unlink_error = match retry_interrupted(|| unlinkat(directory, name, AtFlags::empty())) {
        Ok(()) | Err(Errno::NOENT) => return Ok(()),
        Err(error @ (Errno::ISDIR | Errno::PERM | Errno::ACCESS)) => error,
        Err(error) => return Err(error),
    };
    match drain(directory, name)? {
        Drained::NotADirectory => Err(unlink_error),
        Drained::Gone => Ok(()),
        Drained::Empty => remove_empty(directory, name),
    }
}

fn remove_empty(directory: BorrowedFd<'_>, name: &CStr) -> Result<(), Errno> {
    match retry_interrupted(|| unlinkat(directory, name, AtFlags::REMOVEDIR)) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error),
    }
}

fn drain(directory: BorrowedFd<'_>, name: &CStr) -> Result<Drained, Errno> {
    loop {
        let child = match retry_interrupted(|| {
            openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
        }) {
            Ok(child) => child,
            Err(Errno::NOENT) => return Ok(Drained::Gone),
            Err(Errno::NOTDIR) => return Ok(Drained::NotADirectory),
            Err(error) => return Err(error),
        };
        let names = entry_names(&child, REMOVE_ALL_BATCH)?;
        if names.is_empty() {
            return Ok(Drained::Empty);
        }
        let removed: Vec<Result<(), Errno>> = names.iter().map(|name| remove_all_at(child.as_fd(), name)).collect();
        removed.into_iter().collect::<Result<(), Errno>>()?;
    }
}
