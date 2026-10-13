use std::fs::{File, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::fs::{create_temp_in, parent, path_error};

const TEMP_PREFIX: &str = ".atomic-write-";
const COMMITTED: &str = "atomic write committed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicWriteError {
    pub message: String,
    pub committed: bool,
}

impl AtomicWriteError {
    fn failed(message: String) -> Self {
        Self {
            message,
            committed: false,
        }
    }

    fn committed(message: String) -> Self {
        Self {
            message,
            committed: true,
        }
    }
}

pub fn replace_file(path: &Path, content: &[u8]) -> Result<(), AtomicWriteError> {
    let existing =
        std::fs::symlink_metadata(path).map_err(|e| AtomicWriteError::failed(path_error("lstat", path, &e)))?;
    if !existing.is_file() {
        return Err(AtomicWriteError::failed(format!(
            "atomic write target {:?} is not a regular file",
            path.display()
        )));
    }
    let dir = parent(path);
    stage(&dir, content, &existing.permissions())
        .and_then(|staged| staged.persist(path).map_err(|e| path_error("rename", path, &e.error)))
        .map_err(AtomicWriteError::failed)?;
    sync_directory(&dir).map_err(|e| AtomicWriteError::committed(format!("{COMMITTED}: sync parent directory: {e}")))
}

fn stage(dir: &Path, content: &[u8], permissions: &Permissions) -> Result<tempfile::NamedTempFile, String> {
    let mut staged = create_temp_in(dir, TEMP_PREFIX, "")?;
    let temp = staged.path().to_owned();
    let failed = |op: &'static str| {
        let temp = temp.clone();
        move |e: std::io::Error| path_error(op, &temp, &e)
    };
    staged.write_all(content).map_err(failed("write"))?;
    staged
        .as_file()
        .set_permissions(Permissions::from_mode(permissions.mode() & 0o777))
        .map_err(failed("chmod"))?;
    staged.as_file().sync_all().map_err(failed("sync"))?;
    Ok(staged)
}

fn sync_directory(dir: &Path) -> Result<(), String> {
    File::open(dir)
        .map_err(|e| path_error("open", dir, &e))
        .and_then(|dir_file| dir_file.sync_all().map_err(|e| path_error("sync", dir, &e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_file_is_replaced_keeping_its_mode_and_no_temp_file_stays() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("metadata.json");
        std::fs::write(&file, b"old").unwrap();
        std::fs::set_permissions(&file, Permissions::from_mode(0o640)).unwrap();

        replace_file(&file, b"new").unwrap();

        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_missing_target_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("metadata.json");

        let error = replace_file(&file, b"new").unwrap_err();

        assert_eq!(
            error,
            AtomicWriteError {
                message: format!("lstat {}: No such file or directory (os error 2)", file.display()),
                committed: false,
            }
        );
        assert!(!file.exists());
    }

    #[test]
    fn a_directory_target_is_rejected() {
        let dir = tempfile::tempdir().unwrap();

        let error = replace_file(dir.path(), b"new").unwrap_err();

        assert!(error.message.contains("is not a regular file"), "{}", error.message);
        assert!(!error.committed);
    }
}
