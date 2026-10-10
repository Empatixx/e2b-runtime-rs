use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::util::fs::os_error;

pub const DEFAULT_TARGET_FILE: &str = "/run/e2b/cpu-target";
const MAX_TARGET_BYTES: u64 = 64;

pub struct TargetFile {
    path: PathBuf,
}

impl TargetFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self, count: usize) -> Result<(), String> {
        let directory = self.path.parent().unwrap_or(Path::new("/"));
        fs::create_dir_all(directory).map_err(|e| os_error("mkdir", directory, &e))?;
        let temporary = self.temporary();
        fs::write(&temporary, count.to_string()).map_err(|e| os_error("open", &temporary, &e))?;
        fs::rename(&temporary, &self.path).map_err(|e| {
            format!(
                "rename {} {}: {}",
                temporary.display(),
                self.path.display(),
                crate::util::fs::reason(&e)
            )
        })
    }

    pub fn load(&self) -> Result<Option<i64>, String> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(os_error("open", &self.path, &e)),
        };
        let mut content = String::new();
        file.take(MAX_TARGET_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|e| os_error("read", &self.path, &e))?;
        if content.len() as u64 > MAX_TARGET_BYTES {
            return Err(format!("{}: longer than {MAX_TARGET_BYTES} bytes", self.path.display()));
        }
        content
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| format!("{}: invalid syntax", self.path.display()))
    }

    fn temporary(&self) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(".tmp");
        PathBuf::from(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_file(directory: &tempfile::TempDir) -> TargetFile {
        TargetFile::new(directory.path().join("run").join("cpu-target"))
    }

    #[test]
    fn a_saved_target_loads_back() {
        let directory = tempfile::tempdir().unwrap();
        let file = target_file(&directory);

        file.save(12).unwrap();

        assert_eq!(file.load(), Ok(Some(12)));
        assert!(!file.temporary().exists());
    }

    #[test]
    fn a_missing_file_is_no_target() {
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(target_file(&directory).load(), Ok(None));
    }

    #[test]
    fn content_that_is_not_a_number_or_too_long_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let file = target_file(&directory);
        file.save(1).unwrap();
        for content in ["lots\n", "", &"1".repeat(1 << 20)] {
            fs::write(file.path(), content).unwrap();
            assert!(file.load().is_err(), "{:.8}", content);
        }
    }

    #[test]
    fn whitespace_and_signs_around_the_digits_are_accepted_like_atoi() {
        let directory = tempfile::tempdir().unwrap();
        let file = target_file(&directory);
        file.save(1).unwrap();
        fs::write(file.path(), " 7\n").unwrap();
        assert_eq!(file.load(), Ok(Some(7)));
        fs::write(file.path(), "-3").unwrap();
        assert_eq!(file.load(), Ok(Some(-3)));
    }
}
