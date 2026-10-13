use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

pub fn path_error(op: &str, path: &Path, err: &io::Error) -> String {
    format!("{op} {}: {err}", path.display())
}

pub fn create_temp(prefix: &str, suffix: &str) -> Result<NamedTempFile, String> {
    tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .tempfile()
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
    DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .map_err(|e| path_error("mkdir", path, &e))
}

pub fn parent(path: &Path) -> PathBuf {
    path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mkdir_all_fails_under_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();

        assert!(mkdir_all(&file.join("a/b"), 0o700).is_err());
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
    fn path_errors_name_the_operation_and_path() {
        let error = io::Error::from_raw_os_error(nix::libc::EACCES);

        assert!(path_error("mkdir", Path::new("/x"), &error).starts_with("mkdir /x: "));
    }
}
