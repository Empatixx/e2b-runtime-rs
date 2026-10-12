use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use rustix::io::Errno;

use super::error::PathError;

pub struct ConfinedFile {
    file: File,
    name: String,
}

impl ConfinedFile {
    pub fn new(fd: OwnedFd, name: String) -> Self {
        Self {
            file: File::from(fd),
            name,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, PathError> {
        loop {
            match self.file.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result.map_err(|error| self.failed("read", &error)),
            }
        }
    }

    pub fn write_all(&mut self, content: &[u8]) -> Result<(), PathError> {
        self.file
            .write_all(content)
            .map_err(|error| self.failed("write", &error))
    }

    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> Result<usize, PathError> {
        loop {
            match self.file.read_at(buffer, offset) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result.map_err(|error| self.failed("read", &error)),
            }
        }
    }

    pub fn write_all_at(&self, content: &[u8], offset: u64) -> Result<(), PathError> {
        self.file
            .write_all_at(content, offset)
            .map_err(|error| self.failed("write", &error))
    }

    pub fn set_len(&self, size: u64) -> Result<(), PathError> {
        self.file.set_len(size).map_err(|error| self.failed("truncate", &error))
    }

    pub fn sync(&self) -> Result<(), PathError> {
        self.file.sync_all().map_err(|error| self.failed("sync", &error))
    }

    fn failed(&self, operation: &str, error: &io::Error) -> PathError {
        PathError::builder()
            .operation(operation)
            .path(self.name.as_str())
            .errno(error.raw_os_error().map_or(Errno::IO, Errno::from_raw_os_error))
            .build()
    }
}
