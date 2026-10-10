use std::os::fd::OwnedFd;

use rustix::fs::{AtFlags, FileType, statat};
use rustix::io::Errno;

use super::syscalls::retry_interrupted;

pub struct Parent {
    pub fd: OwnedFd,
    pub base: String,
    pub trailing_slash: bool,
}

impl Parent {
    pub fn require_directory(&self) -> Result<(), Errno> {
        match self.trailing_slash {
            false => Ok(()),
            true => {
                retry_interrupted(|| statat(&self.fd, self.base.as_str(), AtFlags::SYMLINK_NOFOLLOW)).and_then(|stat| {
                    match FileType::from_raw_mode(stat.st_mode) {
                        FileType::Directory => Ok(()),
                        _ => Err(Errno::NOTDIR),
                    }
                })
            }
        }
    }
}
