use std::path::{Path, PathBuf};

const CREDENTIALS_ENV: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const CONFIG_DIRECTORY_ENV: &str = "CLOUDSDK_CONFIG";
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
            None => Ok(well_known_path(env).and_then(|path| Self::read(&path, false).ok())),
        }
    }

    fn read(path: &Path, from_environment: bool) -> Result<Self, String> {
        std::fs::read(path)
            .map(|bytes| Self {
                path: path.to_path_buf(),
                bytes,
                from_environment,
            })
            .map_err(|e| crate::go_os::path_error("open", path, &e))
    }
}

fn well_known_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match env(CONFIG_DIRECTORY_ENV).filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(Path::new(&dir).join(WELL_KNOWN_FILE)),
        None => env("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| Path::new(&home).join(".config/gcloud").join(WELL_KNOWN_FILE)),
    }
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
        std::fs::write(dir.path().join(WELL_KNOWN_FILE), b"well-known").unwrap();

        let located = AdcFile::locate(&env_of(vec![
            (CREDENTIALS_ENV, file.display().to_string()),
            (CONFIG_DIRECTORY_ENV, dir.path().display().to_string()),
        ]))
        .unwrap()
        .unwrap();

        assert_eq!(located.bytes, b"{}");
        assert!(located.from_environment);
        assert_eq!(
            AdcFile::locate(&env_of(vec![(CREDENTIALS_ENV, "/nonexistent/c.json".to_owned())])).unwrap_err(),
            "open /nonexistent/c.json: no such file or directory"
        );
    }

    #[test]
    fn the_well_known_file_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let empty = AdcFile::locate(&env_of(vec![(CONFIG_DIRECTORY_ENV, dir.path().display().to_string())]));
        assert_eq!(empty, Ok(None));

        std::fs::write(dir.path().join(WELL_KNOWN_FILE), b"x").unwrap();
        let found = AdcFile::locate(&env_of(vec![(CONFIG_DIRECTORY_ENV, dir.path().display().to_string())]))
            .unwrap()
            .unwrap();

        assert!(!found.from_environment);
        assert_eq!(found.bytes, b"x");
    }
}
