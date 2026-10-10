use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::firecracker::kernel_args::render;
use crate::template::metadata::CpuTemplate;

pub const INDEX_VERSION: &str = "v2";
const GO_NIL: &str = "<nil>";
const FILES_HASH_LENGTH: usize = 64;

pub fn hash_keys<'a>(base: &str, keys: impl IntoIterator<Item = &'a str>) -> String {
    let digest = keys
        .into_iter()
        .fold(Sha256::new_with_prefix(base.as_bytes()), |sha, key| {
            sha.chain_update(b";").chain_update(key.as_bytes())
        })
        .finalize();
    format!("{digest:x}")
}

const ENVD_MEMORY_PROTECTION_KEY: &str = "envd-memory-protection";
pub const ENVD_MEMORY_MIN_MIB: i64 = 128;
pub const ENVD_MEMORY_LOW_MIB: i64 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseKey<'a> {
    pub provision_version: &'a str,
    pub disk_size_mb: i64,
    pub source: &'a str,
    pub cmdline_args: &'a BTreeMap<String, String>,
    pub cpu_template: Option<&'a CpuTemplate>,
    pub envd_memory_protection: bool,
}

impl BaseKey<'_> {
    pub fn hash(&self) -> String {
        let disk_size_mb = self.disk_size_mb.to_string();
        let cmdline = (!self.cmdline_args.is_empty()).then(|| format!("cmdline:{}", render(self.cmdline_args)));
        let cpu_template = self
            .cpu_template
            .filter(|template| !template.is_empty())
            .map(|template| format!("cpu-template:{}", template.canonical()));
        let protection = self
            .envd_memory_protection
            .then(|| format!("{ENVD_MEMORY_PROTECTION_KEY}:{ENVD_MEMORY_MIN_MIB}:{ENVD_MEMORY_LOW_MIB}"));
        let keys = [self.provision_version, &disk_size_mb, self.source].into_iter().chain(
            [&cmdline, &cpu_template, &protection]
                .into_iter()
                .flatten()
                .map(String::as_str),
        );
        hash_keys(INDEX_VERSION, keys)
    }
}

pub fn validate_files_hash(hash: &str) -> Result<(), String> {
    match hash.len() == FILES_HASH_LENGTH && hash.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
        true => Ok(()),
        false => Err(format!("invalid files hash: {}", crate::go_strconv::quote(hash))),
    }
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

pub fn ensure_free_disk_hash(previous: &str, free_disk_size_mb: i64) -> String {
    hash_keys(previous, ["resize-disk", &free_disk_size_mb.to_string()])
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

    fn base_key<'a>(cmdline_args: &'a BTreeMap<String, String>, cpu_template: Option<&'a CpuTemplate>) -> BaseKey<'a> {
        BaseKey {
            provision_version: "7",
            disk_size_mb: 1024,
            source: "debian:12",
            cmdline_args,
            cpu_template,
            envd_memory_protection: false,
        }
    }

    fn tsc(khz: i64) -> CpuTemplate {
        CpuTemplate {
            x86_tsc_khz: khz,
            ..CpuTemplate::default()
        }
    }

    #[test]
    fn cmdline_args_enter_the_base_key_only_when_set() {
        let none = BTreeMap::new();
        let psi = BTreeMap::from([("psi".to_owned(), "1".to_owned())]);
        let nokaslr = BTreeMap::from([("nokaslr".to_owned(), String::new())]);
        let several = BTreeMap::from([
            ("psi".to_owned(), "1".to_owned()),
            ("nokaslr".to_owned(), String::new()),
            ("a".to_owned(), "b".to_owned()),
        ]);

        assert_eq!(base_key(&none, None).hash(), sha("v2;7;1024;debian:12"));
        assert_eq!(base_key(&psi, None).hash(), sha("v2;7;1024;debian:12;cmdline:psi=1"));
        assert_ne!(base_key(&psi, None).hash(), base_key(&nokaslr, None).hash());
        assert_eq!(
            base_key(&several, None).hash(),
            sha("v2;7;1024;debian:12;cmdline:a=b nokaslr psi=1")
        );
    }

    #[test]
    fn a_cpu_template_enters_the_base_key_only_when_it_changes_something() {
        let none = BTreeMap::new();
        let psi = BTreeMap::from([("psi".to_owned(), "1".to_owned())]);
        let fast = tsc(3_200_000);
        let slow = tsc(2_400_000);
        let with_capability = CpuTemplate {
            kvm_capabilities: vec!["!121".to_owned()],
            ..tsc(3_200_000)
        };
        let reordered = CpuTemplate::parse(r#"{"kvm_capabilities":["!121"],"x86_tsc_khz":3200000}"#)
            .unwrap()
            .unwrap();
        let empty = CpuTemplate::default();

        assert_eq!(
            base_key(&none, Some(&fast)).hash(),
            sha("v2;7;1024;debian:12;cpu-template:{\"x86_tsc_khz\":3200000}")
        );
        assert_eq!(base_key(&none, Some(&empty)).hash(), base_key(&none, None).hash());
        assert_ne!(base_key(&none, Some(&fast)).hash(), base_key(&none, Some(&slow)).hash());
        assert_ne!(
            base_key(&none, Some(&fast)).hash(),
            base_key(&none, Some(&with_capability)).hash()
        );
        assert_eq!(
            base_key(&none, Some(&with_capability)).hash(),
            base_key(&none, Some(&reordered)).hash()
        );
        assert_ne!(base_key(&psi, Some(&fast)).hash(), base_key(&psi, None).hash());
        assert_ne!(base_key(&psi, Some(&fast)).hash(), base_key(&none, Some(&fast)).hash());
    }

    #[test]
    fn phase_hashes_use_upstream_keys() {
        let none = BTreeMap::new();
        let key = base_key(&none, None);
        assert_eq!(key.hash(), sha("v2;7;1024;debian:12"));
        assert_eq!(
            BaseKey {
                envd_memory_protection: true,
                ..key
            }
            .hash(),
            sha("v2;7;1024;debian:12;envd-memory-protection:128:256")
        );
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
        assert_eq!(ensure_free_disk_hash("p", 2048), sha("p;resize-disk;2048"));
        assert_eq!(ensure_free_disk_hash("p", 0), sha("p;resize-disk;0"));
        assert_eq!(template_source("b1"), "template:b1");
    }
}
