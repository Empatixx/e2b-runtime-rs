use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

pub fn path_error(op: &str, path: &Path, err: &io::Error) -> String {
    format!("{op} {}: {}", path.display(), reason(err))
}

pub fn create_temp(prefix: &str, suffix: &str) -> Result<NamedTempFile, String> {
    tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .make(|path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| match e.kind() {
                    io::ErrorKind::AlreadyExists => e,
                    _ => io::Error::other(path_error("open", path, &e)),
                })
        })
        .map_err(|e| e.to_string())
}

pub fn create_temp_in(dir: &Path, prefix: &str, suffix: &str) -> Result<NamedTempFile, String> {
    tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .tempfile_in(dir)
        .map_err(|e| path_error("open", dir, &e))
}

pub fn parent(path: &Path) -> PathBuf {
    path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

pub fn reason(err: &io::Error) -> String {
    err.raw_os_error()
        .map(|code| go_errno_text(code).unwrap_or_else(|| nix::errno::Errno::from_raw(code).desc()))
        .map(|desc| {
            let first = desc.chars().take(1).flat_map(char::to_lowercase);
            first.chain(desc.chars().skip(1)).collect()
        })
        .unwrap_or_else(|| err.to_string())
}

fn go_errno_text(code: i32) -> Option<&'static str> {
    match code {
        nix::libc::EOPNOTSUPP => Some("operation not supported"),
        nix::libc::EXDEV => Some("invalid cross-device link"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_like_go_path_errors() {
        let error = io::Error::from_raw_os_error(nix::libc::EACCES);

        assert_eq!(
            path_error("mkdir", Path::new("/x"), &error),
            "mkdir /x: permission denied"
        );
    }
}
