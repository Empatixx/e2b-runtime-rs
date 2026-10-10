use std::fs::Metadata;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub size: u64,
    pub modified: SystemTime,
}

impl Identity {
    pub fn of(metadata: &Metadata) -> std::io::Result<Self> {
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified()?,
        })
    }

    pub fn unix_nanos(&self) -> i128 {
        match self.modified.duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_nanos() as i128,
            Err(before) => -(before.duration().as_nanos() as i128),
        }
    }

    pub fn key(&self, source: &Path) -> String {
        format!("{}\0{}\0{}", source.display(), self.size, self.unix_nanos())
    }

    pub fn local_name(&self, source: &Path) -> String {
        let digest = Sha256::digest(source.as_os_str().as_encoded_bytes());
        let base = source
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        format!(
            "{base}.{}.{}.{}",
            self.size,
            self.unix_nanos(),
            hex::encode(&digest[..4])
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(seconds: u64, size: u64) -> Identity {
        Identity {
            size,
            modified: UNIX_EPOCH + Duration::from_secs(seconds),
        }
    }

    #[test]
    fn the_local_name_encodes_the_whole_key() {
        let source = Path::new("/fc-envd/envd");

        let first = at(1_700_000_000, 4).local_name(source);
        let second = at(1_700_000_500, 4).local_name(source);

        assert_ne!(
            first, second,
            "equal size, different mtime: the names must still differ"
        );
        assert!(
            first.starts_with("envd."),
            "a copy's name must identify the source it came from: {first}"
        );
    }

    #[test]
    fn sources_sharing_basename_size_and_mtime_get_different_names() {
        let identity = at(1_700_000_000, 8);

        let one = identity.local_name(Path::new("/a/envd"));
        let two = identity.local_name(Path::new("/b/envd"));

        assert_ne!(one, two);
    }

    #[test]
    fn the_identity_key_separates_path_size_and_mtime() {
        let source = Path::new("/fc-envd/envd");

        assert_ne!(at(1, 9).key(source), at(2, 9).key(source));
        assert_ne!(at(1, 9).key(source), at(1, 10).key(source));
        assert_ne!(at(1, 9).key(source), at(1, 9).key(Path::new("/fc-envd/envd.v2")));
        assert_eq!(at(1, 9).key(source), at(1, 9).key(source));
    }
}
