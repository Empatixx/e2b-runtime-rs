use rustix::fs::{Mode, OFlags, Stat, fstat};

use super::Chrooted;
use super::error::{PathError, failed};
use super::file::ConfinedFile;
use super::syscalls::retry_interrupted;

impl Chrooted {
    pub fn lstat_raw(&self, name: &str) -> Result<Stat, PathError> {
        let fd = self.open_at("lstat", name, OFlags::PATH | OFlags::NOFOLLOW, Mode::empty())?;
        retry_interrupted(|| fstat(&fd)).map_err(failed("lstat", name))
    }

    pub fn create_or_open(&self, name: &str, permissions: u32) -> Result<ConfinedFile, PathError> {
        self.open_confined(name, OFlags::CREATE | OFlags::WRONLY, permissions)
    }

    pub fn open_read_write(&self, name: &str) -> Result<ConfinedFile, PathError> {
        self.open_confined(name, OFlags::RDWR, 0)
    }

    pub fn truncate(&self, name: &str, size: u64) -> Result<(), PathError> {
        self.open_confined(name, OFlags::WRONLY, 0)?.set_len(size)
    }
}
