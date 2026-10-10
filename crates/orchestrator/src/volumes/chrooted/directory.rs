use std::ffi::CString;
use std::os::fd::AsFd;

use rustix::fs::Dir;
use rustix::io::Errno;

pub fn entry_names(directory: impl AsFd, limit: usize) -> Result<Vec<CString>, Errno> {
    Dir::read_from(directory)?
        .map(|entry| entry.map(|entry| entry.file_name().to_owned()))
        .filter(|name| !matches!(name, Ok(name) if is_dot_entry(name.to_bytes())))
        .take(limit)
        .collect()
}

fn is_dot_entry(name: &[u8]) -> bool {
    name == b"." || name == b".."
}
