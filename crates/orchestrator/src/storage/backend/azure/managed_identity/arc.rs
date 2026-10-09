use std::path::Path;

const LINUX_TOKEN_DIRECTORY: &str = "/var/opt/azcmagent/tokens";
const KEY_EXTENSION: &str = ".key";
const MAX_KEY_BYTES: u64 = 4096;
const REALM_MARKER: &str = "Basic realm=";

pub fn token_directory(platform: &str) -> Option<&'static str> {
    (platform == "linux").then_some(LINUX_TOKEN_DIRECTORY)
}

pub fn secret_key(challenge: &str, directory: Option<&str>) -> Result<String, String> {
    if challenge.is_empty() {
        return Err("response has no www-authenticate header".to_owned());
    }
    let directory = directory.ok_or_else(|| "platform not supported, expected linux or windows".to_owned())?;
    let path = challenge
        .split_once(REALM_MARKER)
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("basic realm= not found in the string, instead found: {challenge}"))?;
    let location = Path::new(path);
    validate_extension(location)?;
    validate_directory(location, directory)?;
    read_key(location)
}

fn validate_extension(path: &Path) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.ends_with(KEY_EXTENSION) {
        true => Ok(()),
        false => Err(format!(
            "invalid file extension, expected {KEY_EXTENSION}, got {}",
            name.rfind('.').map_or("", |index| &name[index..])
        )),
    }
}

fn validate_directory(path: &Path, expected: &str) -> Result<(), String> {
    let actual = path
        .parent()
        .map_or_else(|| ".".to_owned(), |parent| parent.display().to_string());
    match actual == expected {
        true => Ok(()),
        false => Err(format!("invalid file path, expected {expected}, got {actual}")),
    }
}

fn read_key(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        format!(
            "failed to get metadata for {} due to error: {}",
            path.display(),
            crate::go_os::path_error("stat", path, &e)
        )
    })?;
    if metadata.len() > MAX_KEY_BYTES {
        return Err(format!(
            "invalid secret file size, expected {MAX_KEY_BYTES}, file size was {}",
            metadata.len()
        ));
    }
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| {
            format!(
                "failed to read {} due to error: {}",
                crate::go_strconv::quote(&path.display().to_string()),
                crate::go_os::path_error("open", path, &e)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(path: &Path) -> String {
        format!("Basic realm={}", path.display())
    }

    #[test]
    fn only_linux_has_a_token_directory() {
        assert_eq!(token_directory("linux"), Some("/var/opt/azcmagent/tokens"));
        assert_eq!(token_directory("macos"), None);
    }

    #[test]
    fn the_key_is_read_from_the_file_named_by_the_challenge() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("abc.key");
        std::fs::write(&file, "the-secret").unwrap();

        let key = secret_key(&challenge(&file), directory.path().to_str());

        assert_eq!(key.unwrap(), "the-secret");
    }

    #[test]
    fn challenges_are_validated_in_msal_order() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().to_str();
        let other = tempfile::tempdir().unwrap();

        assert_eq!(
            secret_key("", expected).unwrap_err(),
            "response has no www-authenticate header"
        );
        assert_eq!(
            secret_key("Basic realm=/x.key", None).unwrap_err(),
            "platform not supported, expected linux or windows"
        );
        assert_eq!(
            secret_key("Bearer x", expected).unwrap_err(),
            "basic realm= not found in the string, instead found: Bearer x"
        );
        assert_eq!(
            secret_key("Basic realm=/tmp/a.txt", expected).unwrap_err(),
            "invalid file extension, expected .key, got .txt"
        );
        assert_eq!(
            secret_key(&challenge(&other.path().join("a.key")), expected).unwrap_err(),
            format!(
                "invalid file path, expected {}, got {}",
                directory.path().display(),
                other.path().display()
            )
        );
    }

    #[test]
    fn missing_oversized_and_unreadable_files_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().to_str();
        let missing = directory.path().join("missing.key");
        let big = directory.path().join("big.key");
        std::fs::write(&big, vec![b'x'; 4097]).unwrap();

        let missing_error = secret_key(&challenge(&missing), expected).unwrap_err();
        let big_error = secret_key(&challenge(&big), expected).unwrap_err();

        assert!(missing_error.starts_with(&format!(
            "failed to get metadata for {} due to error: stat {}: no such file or directory",
            missing.display(),
            missing.display()
        )));
        assert_eq!(big_error, "invalid secret file size, expected 4096, file size was 4097");
    }
}
