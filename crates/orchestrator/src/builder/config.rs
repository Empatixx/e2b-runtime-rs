mod envd_binary;
mod flags;
mod settings;

use std::collections::HashMap;
use std::path::PathBuf;

pub use envd_binary::{envd_version, resolve_build_binary};
pub use flags::BuildFlags;
pub use settings::BuildSettings;

use crate::featureflags::FeatureFlags;

const DEFAULT_BUSYBOX_VERSION: &str = "1.36.1";
const DEFAULT_BUSYBOX_DIR: &str = "/fc-busybox";
const DEFAULT_ENVD_PATH: &str = "/fc-envd/envd";
const DEFAULT_BASE_PATH: &str = "/orchestrator";
const DEFAULT_TEMPLATES_DIR: &str = "${ORCHESTRATOR_BASE_PATH}/build-templates";
const DEFAULT_PROVIDER: &str = "gcp";
const DEFAULT_DOCKERHUB_PROVIDER: &str = "GCP_REMOTE_REPOSITORY";
const DEFAULT_KERNEL_VERSION: &str = "vmlinux-6.1.158";
const DEFAULT_FIRECRACKER_VERSION: &str = "v1.14-0.2.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderConfig {
    pub busybox: PathBuf,
    pub envd: PathBuf,
    pub templates_dir: PathBuf,
    pub provider: String,
    pub target_arch: String,
    pub dockerhub_provider: String,
    pub dockerhub_url: String,
    pub kernel_version: String,
    pub firecracker_version: String,
    pub feature_flags: FeatureFlags,
}

impl BuilderConfig {
    pub fn from_vars(vars: &HashMap<String, String>) -> Self {
        let var = |name: &str, default: &str| {
            vars.get(name)
                .filter(|value| !value.is_empty())
                .map_or_else(|| default.to_owned(), Clone::clone)
        };
        let with_base = |name: &str, default: &str| {
            let base = var("ORCHESTRATOR_BASE_PATH", DEFAULT_BASE_PATH);
            expand(&var(name, default), &|key| match key {
                "ORCHESTRATOR_BASE_PATH" => base.clone(),
                key => vars.get(key).cloned().unwrap_or_default(),
            })
        };
        Self {
            busybox: [
                absolute(var("HOST_BUSYBOX_DIR", DEFAULT_BUSYBOX_DIR).into())
                    .display()
                    .to_string(),
                var("BUSYBOX_VERSION", DEFAULT_BUSYBOX_VERSION),
                host_arch().to_owned(),
                "busybox".to_owned(),
            ]
            .iter()
            .collect(),
            envd: absolute(var("HOST_ENVD_PATH", DEFAULT_ENVD_PATH).into()),
            templates_dir: absolute(with_base("TEMPLATES_DIR", DEFAULT_TEMPLATES_DIR).into()),
            provider: var("PROVIDER", DEFAULT_PROVIDER),
            target_arch: target_arch(vars.get("TARGET_ARCH").map(String::as_str)),
            dockerhub_provider: var("DOCKERHUB_REMOTE_REPOSITORY_PROVIDER", DEFAULT_DOCKERHUB_PROVIDER),
            dockerhub_url: var("DOCKERHUB_REMOTE_REPOSITORY_URL", ""),
            kernel_version: var("DEFAULT_KERNEL_VERSION", DEFAULT_KERNEL_VERSION),
            firecracker_version: var("DEFAULT_FIRECRACKER_VERSION", DEFAULT_FIRECRACKER_VERSION),
            feature_flags: FeatureFlags::offline(),
        }
    }
}

pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn target_arch(value: Option<&str>) -> String {
    match value.filter(|value| !value.is_empty()) {
        None => host_arch().to_owned(),
        Some("amd64" | "x86_64") => "amd64".to_owned(),
        Some("arm64" | "aarch64") => "arm64".to_owned(),
        Some(other) => {
            eprintln!(
                "WARNING: unrecognized TARGET_ARCH={}, falling back to {}",
                crate::go_strconv::quote(other),
                host_arch()
            );
            host_arch().to_owned()
        }
    }
}

fn expand(text: &str, lookup: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        let (name, width) = shell_name(after);
        match (name, width) {
            (None, 0) => out.push('$'),
            (None, _) => {}
            (Some(name), _) => out.push_str(&lookup(name)),
        }
        rest = &after[width..];
    }
    out.push_str(rest);
    out
}

fn shell_name(text: &str) -> (Option<&str>, usize) {
    let bytes = text.as_bytes();
    match bytes.first() {
        None => (None, 0),
        Some(b'{') => braced_name(text),
        Some(c) if is_special(*c) => (Some(&text[..1]), 1),
        Some(_) => {
            let end = bytes
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                .unwrap_or(bytes.len());
            match end {
                0 => (None, 0),
                end => (Some(&text[..end]), end),
            }
        }
    }
}

fn braced_name(text: &str) -> (Option<&str>, usize) {
    let bytes = text.as_bytes();
    if bytes.len() > 2 && is_special(bytes[1]) && bytes[2] == b'}' {
        return (Some(&text[1..2]), 3);
    }
    match text[1..].find('}') {
        Some(0) => (None, 2),
        Some(end) => (Some(&text[1..end + 1]), end + 2),
        None => (None, 1),
    }
}

fn is_special(c: u8) -> bool {
    matches!(c, b'*' | b'#' | b'$' | b'@' | b'!' | b'?' | b'-') || c.is_ascii_digit()
}

fn absolute(path: PathBuf) -> PathBuf {
    match path.is_absolute() || path.as_os_str().is_empty() {
        true => path,
        false => std::env::current_dir().map_or(path.clone(), |cwd| cwd.join(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> BuilderConfig {
        BuilderConfig::from_vars(
            &pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        )
    }

    #[test]
    fn defaults_follow_upstream() {
        let config = config(&[]);

        assert_eq!(
            config.busybox,
            PathBuf::from(format!("/fc-busybox/1.36.1/{}/busybox", host_arch()))
        );
        assert_eq!(config.envd, PathBuf::from("/fc-envd/envd"));
        assert_eq!(config.templates_dir, PathBuf::from("/orchestrator/build-templates"));
        assert_eq!(config.provider, "gcp");
        assert_eq!(config.dockerhub_provider, "GCP_REMOTE_REPOSITORY");
        assert_eq!(config.target_arch, host_arch());
        assert_eq!(
            (config.kernel_version.as_str(), config.firecracker_version.as_str()),
            ("vmlinux-6.1.158", "v1.14-0.2.0")
        );
    }

    #[test]
    fn templates_dir_expands_the_base_path_and_arch_is_normalised() {
        let config = config(&[
            ("ORCHESTRATOR_BASE_PATH", "/a/b"),
            ("TEMPLATES_DIR", "$ORCHESTRATOR_BASE_PATH/t/${HOME_UNSET}x"),
            ("TARGET_ARCH", "aarch64"),
        ]);

        assert_eq!(config.templates_dir, PathBuf::from("/a/b/t/x"));
        let lookup = |name: &str| format!("<{name}>");
        assert_eq!(expand("${abc", &lookup), "abc");
        assert_eq!(expand("$1x", &lookup), "<1>x");
        assert_eq!(expand("$-a", &lookup), "<->a");
        assert_eq!(expand("a$", &lookup), "a$");
        assert_eq!(expand("${}b", &lookup), "b");
        assert_eq!(expand("$/", &lookup), "$/");
        assert!(super::absolute(PathBuf::from("rel")).is_absolute());
        assert_eq!(config.target_arch, "arm64");
        assert_eq!(target_arch(Some("x86_64")), "amd64");
    }
}
