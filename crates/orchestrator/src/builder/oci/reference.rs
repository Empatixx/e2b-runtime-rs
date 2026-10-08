use std::net::Ipv4Addr;

use crate::go_url::GoUrl;

pub const DEFAULT_REGISTRY: &str = "index.docker.io";
const DEFAULT_REGISTRY_ALIAS: &str = "docker.io";
const DEFAULT_NAMESPACE: &str = "library";
const DEFAULT_TAG: &str = "latest";
const REPOSITORY_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789_-./";
const TAG_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789_-.ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGEST_PREFIX: &str = "sha256:";
const SHA256_HEX_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identifier {
    Tag(String),
    Digest(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub identifier: Identifier,
}

impl ImageReference {
    pub fn parse(text: &str) -> Result<Self, String> {
        parse_tag(text)
            .or_else(|| parse_digest(text))
            .ok_or_else(|| format!("could not parse reference: {text}"))
    }

    pub fn repository_str(&self) -> String {
        match self.repository.contains('/') || self.registry != DEFAULT_REGISTRY {
            true => self.repository.clone(),
            false => format!("{DEFAULT_NAMESPACE}/{}", self.repository),
        }
    }

    pub fn identifier(&self) -> &str {
        match &self.identifier {
            Identifier::Tag(tag) => tag,
            Identifier::Digest(digest) => digest,
        }
    }

    pub fn name(&self) -> String {
        let separator = match self.identifier {
            Identifier::Tag(_) => ':',
            Identifier::Digest(_) => '@',
        };
        format!(
            "{}/{}{separator}{}",
            self.registry,
            self.repository_str(),
            self.identifier()
        )
    }

    pub fn without_registry(&self) -> String {
        self.name()
            .trim_start_matches(&self.registry)
            .trim_start_matches('/')
            .to_owned()
    }

    pub fn is_docker_hub(&self) -> bool {
        self.registry == DEFAULT_REGISTRY
    }

    pub fn scheme(&self) -> &'static str {
        let name = self.registry.as_str();
        let host = name.split(':').next().unwrap_or_default();
        let insecure = is_rfc1918(host)
            || name.starts_with("localhost:")
            || is_local(name)
            || is_loopback(name)
            || is_ipv6_loopback(name);
        match insecure {
            true => "http",
            false => "https",
        }
    }
}

fn parse_tag(text: &str) -> Option<ImageReference> {
    let (base, tag) = match text.rsplit_once(':') {
        Some((base, tag)) if !tag.contains('/') => (base, tag),
        _ => (text, DEFAULT_TAG),
    };
    if !check_element(tag, TAG_CHARS, 1, 128) {
        return None;
    }
    let (registry, repository) = parse_repository(base)?;
    Some(ImageReference {
        registry,
        repository,
        identifier: Identifier::Tag(tag.to_owned()),
    })
}

fn parse_digest(text: &str) -> Option<ImageReference> {
    let (base, digest) = match text.split('@').collect::<Vec<_>>().as_slice() {
        [base, digest] => (*base, *digest),
        _ => return None,
    };
    let hex = digest.strip_prefix(DIGEST_PREFIX)?;
    if hex.len() != SHA256_HEX_LEN || !hex.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return None;
    }
    let base = parse_tag(base).map_or_else(
        || base.to_owned(),
        |tagged| format!("{}/{}", tagged.registry, tagged.repository_str()),
    );
    let (registry, repository) = parse_repository(&base)?;
    Some(ImageReference {
        registry,
        repository,
        identifier: Identifier::Digest(digest.to_owned()),
    })
}

fn parse_repository(name: &str) -> Option<(String, String)> {
    if name.is_empty() {
        return None;
    }
    let (registry, repository) = match name.split_once('/') {
        Some((first, rest)) if first == "localhost" || first.contains(['.', ':']) => (first, rest),
        _ => ("", name),
    };
    if !check_element(repository, REPOSITORY_CHARS, 2, 255) || !valid_registry(registry) {
        return None;
    }
    let registry = match registry {
        "" | DEFAULT_REGISTRY_ALIAS => DEFAULT_REGISTRY,
        registry => registry,
    };
    Some((registry.to_owned(), repository.to_owned()))
}

fn valid_registry(name: &str) -> bool {
    GoUrl::parse(&format!("//{name}")).is_ok_and(|url| url.host == name)
}

fn check_element(element: &str, allowed: &str, min: usize, max: usize) -> bool {
    let count = element.chars().count();
    (min..=max).contains(&count) && element.chars().all(|c| allowed.contains(c))
}

fn is_rfc1918(host: &str) -> bool {
    host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_private())
}

fn is_local(name: &str) -> bool {
    strip_port(name).ends_with(".localhost")
}

fn is_loopback(name: &str) -> bool {
    strip_port(name) == "127.0.0.1"
}

fn is_ipv6_loopback(name: &str) -> bool {
    name == "::1" || name == "[::1]" || name.strip_prefix("[::1]:").is_some_and(valid_port)
}

fn strip_port(name: &str) -> &str {
    match name.rsplit_once(':') {
        Some((host, port)) if valid_port(port) => host,
        _ => name,
    }
}

fn valid_port(port: &str) -> bool {
    (1..=5).contains(&port.len()) && port.bytes().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> ImageReference {
        ImageReference::parse(text).unwrap()
    }

    #[test]
    fn docker_hub_names_get_the_library_namespace_and_latest() {
        let ubuntu = parsed("ubuntu:22.04");
        assert_eq!(ubuntu.name(), "index.docker.io/library/ubuntu:22.04");
        assert_eq!(ubuntu.without_registry(), "library/ubuntu:22.04");
        assert!(ubuntu.is_docker_hub());
        assert_eq!(
            parsed("docker.io/library/debian").name(),
            "index.docker.io/library/debian:latest"
        );
        assert_eq!(parsed("e2bdev/base").without_registry(), "e2bdev/base:latest");
    }

    #[test]
    fn registries_ports_and_digests() {
        let custom = parsed("registry.example.com:5000/team/app:v1");
        assert_eq!(
            (custom.registry.as_str(), custom.repository_str()),
            ("registry.example.com:5000", "team/app".to_owned())
        );
        assert!(!custom.is_docker_hub());
        let digest = format!("alpine@sha256:{}", "a".repeat(64));
        assert_eq!(
            parsed(&digest).name(),
            format!("index.docker.io/library/alpine@sha256:{}", "a".repeat(64))
        );
        let tagged_digest = format!("alpine:3@sha256:{}", "b".repeat(64));
        assert_eq!(parsed(&tagged_digest).repository_str(), "library/alpine");
    }

    #[test]
    fn bad_references_fail_like_go() {
        for bad in ["UPPER:tag", "ubuntu:", "", "a", "repo@sha256:abc", "x/y:t!g"] {
            assert_eq!(
                ImageReference::parse(bad).unwrap_err(),
                format!("could not parse reference: {bad}"),
                "{bad}"
            );
        }
    }

    #[test]
    fn schemes_follow_go_containerregistry() {
        assert_eq!(parsed("localhost:5000/a/b").scheme(), "http");
        assert_eq!(parsed("127.0.0.1:5000/a/b").scheme(), "http");
        assert_eq!(parsed("10.1.2.3:5000/a/b").scheme(), "http");
        assert_eq!(parsed("registry.localhost/a/b").scheme(), "http");
        assert_eq!(parsed("ghcr.io/a/b").scheme(), "https");
    }
}
