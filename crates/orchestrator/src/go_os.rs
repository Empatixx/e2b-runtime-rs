use std::fs::{DirBuilder, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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

pub fn mkdir_all(path: &Path, mode: u32) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(found) if found.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(path_error(
                "mkdir",
                path,
                &io::Error::from_raw_os_error(nix::libc::ENOTDIR),
            ));
        }
        Err(_) => {}
    }
    if let Some(parent) = mkdir_parent(path) {
        mkdir_all(parent, mode)?;
    }
    DirBuilder::new().mode(mode).create(path).or_else(|e| {
        match std::fs::symlink_metadata(path).is_ok_and(|found| found.is_dir()) {
            true => Ok(()),
            false => Err(path_error("mkdir", path, &e)),
        }
    })
}

fn mkdir_parent(path: &Path) -> Option<&Path> {
    let bytes = path.as_os_str().as_bytes();
    let trimmed = bytes.iter().rposition(|byte| *byte != b'/').map_or(0, |last| last + 1);
    let cut = bytes[..trimmed].iter().rposition(|byte| *byte == b'/').unwrap_or(0);
    (cut > 0).then(|| Path::new(std::ffi::OsStr::from_bytes(&bytes[..cut])))
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
        nix::libc::EIO => Some("input/output error"),
        nix::libc::ESTALE => Some("stale NFS file handle"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mkdir_all_names_the_first_component_that_failed_like_go() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();

        assert_eq!(
            mkdir_all(&file.join("a/b"), 0o700),
            Err(format!("mkdir {}: not a directory", file.display()))
        );
    }

    #[test]
    fn mkdir_all_creates_every_missing_parent_and_accepts_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a//b/c/");

        assert_eq!(mkdir_all(&nested, 0o700), Ok(()));
        assert!(dir.path().join("a/b/c").is_dir());
        assert_eq!(mkdir_all(&nested, 0o700), Ok(()));
    }

    #[test]
    fn the_mkdir_parent_is_cut_like_go() {
        let parent = |path: &str| mkdir_parent(Path::new(path)).map(|parent| parent.to_str().unwrap().to_owned());

        assert_eq!(parent("/a"), None);
        assert_eq!(parent("a"), None);
        assert_eq!(parent("/a/b"), Some("/a".to_owned()));
        assert_eq!(parent("/a/b//"), Some("/a".to_owned()));
        assert_eq!(parent("a//b"), Some("a/".to_owned()));
        assert_eq!(parent("//a"), Some("/".to_owned()));
    }

    #[test]
    fn reads_like_go_path_errors() {
        let error = io::Error::from_raw_os_error(nix::libc::EACCES);

        assert_eq!(
            path_error("mkdir", Path::new("/x"), &error),
            "mkdir /x: permission denied"
        );
    }

    #[test]
    fn reads_like_the_go_errno_table() {
        for (code, text) in [
            (nix::libc::EIO, "input/output error"),
            (nix::libc::ESTALE, "stale NFS file handle"),
            (nix::libc::EXDEV, "invalid cross-device link"),
        ] {
            assert_eq!(reason(&io::Error::from_raw_os_error(code)), text);
        }
    }
}
