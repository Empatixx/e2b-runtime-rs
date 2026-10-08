use sha2::{Digest, Sha256};

pub const INDEX_VERSION: &str = "v2";
const GO_NIL: &str = "<nil>";

pub fn hash_keys<'a>(base: &str, keys: impl IntoIterator<Item = &'a str>) -> String {
    let digest = keys
        .into_iter()
        .fold(Sha256::new_with_prefix(base.as_bytes()), |sha, key| {
            sha.chain_update(b";").chain_update(key.as_bytes())
        })
        .finalize();
    format!("{digest:x}")
}

pub fn base_hash(provision_version: &str, disk_size_mb: i32, source: &str) -> String {
    hash_keys(INDEX_VERSION, [provision_version, &disk_size_mb.to_string(), source])
}

pub fn user_hash(previous: &str) -> String {
    hash_keys(previous, ["DEFAULT USER", "user"])
}

pub fn step_hash(previous: &str, kind: &str, args: &[String], files_hash: Option<&str>) -> String {
    hash_keys(previous, [kind, &args.join(" "), files_hash.unwrap_or(GO_NIL)])
}

pub fn finalize_hash(previous: &str, configure_script: &str, cert_bundle_command: &str) -> String {
    hash_keys(previous, ["config-run-cmd", configure_script, cert_bundle_command])
}

pub fn optimize_hash(previous: &str) -> String {
    hash_keys(previous, ["optimize"])
}

pub fn template_source(build_id: &str) -> String {
    format!("template:{build_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(text: &str) -> String {
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }

    #[test]
    fn keys_are_joined_with_semicolons_like_upstream() {
        assert_eq!(hash_keys("v2", ["a", "b"]), sha("v2;a;b"));
        assert_eq!(hash_keys("base", []), sha("base"));
    }

    #[test]
    fn phase_hashes_use_upstream_keys() {
        assert_eq!(base_hash("7", 1024, "debian:12"), sha("v2;7;1024;debian:12"));
        assert_eq!(user_hash("p"), sha("p;DEFAULT USER;user"));
        assert_eq!(
            step_hash("p", "RUN", &["echo".to_owned(), "hi".to_owned()], None),
            sha("p;RUN;echo hi;<nil>")
        );
        assert_eq!(step_hash("p", "COPY", &[], Some("f")), sha("p;COPY;;f"));
        assert_eq!(
            finalize_hash("p", "script", "certs"),
            sha("p;config-run-cmd;script;certs")
        );
        assert_eq!(optimize_hash("p"), sha("p;optimize"));
        assert_eq!(template_source("b1"), "template:b1");
    }
}
