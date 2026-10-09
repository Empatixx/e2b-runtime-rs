use sha2::{Digest, Sha256};

use super::{DISTRO_SELECTOR, PROVISION_SCRIPT, SYMLINKS, TEMPLATES};
use crate::builder::hash::hash_keys;

const DISTRO_VERSION: &str = "1";

pub fn files_hash() -> String {
    let mut templates = TEMPLATES.to_vec();
    templates.sort_by_key(|(name, _)| *name);
    let mut symlinks = SYMLINKS.to_vec();
    symlinks.sort();
    let digest = symlinks.iter().fold(
        templates.iter().fold(Sha256::new(), |sha, (name, source)| {
            sha.chain_update(format!("{name}\0{}\0", hex::encode(source)))
        }),
        |sha, (name, target)| sha.chain_update(format!("{name}\0{target}\0")),
    );
    format!("{:x}", digest.finalize())
}

pub fn distro_fingerprint() -> String {
    format!("{:x}", Sha256::digest(format!("{DISTRO_VERSION}\0{DISTRO_SELECTOR}")))
}

pub fn provision_version() -> String {
    hash_keys(PROVISION_SCRIPT, [files_hash().as_str(), distro_fingerprint().as_str()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_upstream() {
        assert!(files_hash().starts_with("e5641b1d") && files_hash().ends_with("45d79339"));
        assert!(distro_fingerprint().starts_with("f6af3e80") && distro_fingerprint().ends_with("391a169e"));
        assert_eq!(
            provision_version(),
            "83b23b6a510b214cdfc5bce2a379219610df77a4e4fecc1c7598fcd17caa08f7"
        );
    }
}
