use std::path::{Path, PathBuf};

const CREDENTIALS_ENV: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const WELL_KNOWN_FILE: &str = "application_default_credentials.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdcFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub from_environment: bool,
}

impl AdcFile {
    pub fn locate(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        match env(CREDENTIALS_ENV).filter(|path| !path.is_empty()) {
            Some(path) => Self::read(Path::new(&path), true).map(Some),
            None => Ok(Self::read(&well_known_path(env), false).ok()),
        }
    }

    fn read(path: &Path, from_environment: bool) -> Result<Self, String> {
        std::fs::read(path)
            .map(|bytes| Self {
                path: path.to_path_buf(),
                bytes,
                from_environment,
            })
            .map_err(|e| crate::fs::path_error("open", path, &e))
    }
}

fn well_known_path(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let home = env("HOME")
        .filter(|home| !home.is_empty())
        .or_else(current_user_home)
        .unwrap_or_default();
    Path::new(&home).join(".config/gcloud").join(WELL_KNOWN_FILE)
}

pub fn current_user_home() -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|user| user.dir.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn the_environment_file_wins_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("creds.json");
        std::fs::write(&file, b"{}").unwrap();
        std::fs::create_dir_all(dir.path().join(".config/gcloud")).unwrap();
        std::fs::write(dir.path().join(".config/gcloud").join(WELL_KNOWN_FILE), b"well-known").unwrap();

        let located = AdcFile::locate(&env_of(vec![
            (CREDENTIALS_ENV, file.display().to_string()),
            ("HOME", dir.path().display().to_string()),
        ]))
        .unwrap()
        .unwrap();

        assert_eq!(located.bytes, b"{}");
        assert!(located.from_environment);
        assert_eq!(
            AdcFile::locate(&env_of(vec![(CREDENTIALS_ENV, "/nonexistent/c.json".to_owned())])).unwrap_err(),
            "open /nonexistent/c.json: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn the_well_known_file_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let empty = AdcFile::locate(&env_of(vec![("HOME", dir.path().display().to_string())]));
        assert_eq!(empty, Ok(None));

        std::fs::create_dir_all(dir.path().join(".config/gcloud")).unwrap();
        std::fs::write(dir.path().join(".config/gcloud").join(WELL_KNOWN_FILE), b"x").unwrap();
        let found = AdcFile::locate(&env_of(vec![("HOME", dir.path().display().to_string())]))
            .unwrap()
            .unwrap();

        assert!(!found.from_environment);
        assert_eq!(found.bytes, b"x");
    }
}
