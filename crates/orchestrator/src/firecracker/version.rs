use semver::Version;

pub fn supports_memfd(firecracker_version: &str) -> bool {
    release_line(firecracker_version).is_some_and(|release| (release.major, release.minor) >= (1, 14))
}

fn release_line(version: &str) -> Option<Version> {
    match version.split_once('_') {
        Some((release, _)) => strict_semver(release),
        None => e2b_release(version).or_else(|| strict_semver(version)),
    }
}

fn e2b_release(version: &str) -> Option<Version> {
    let (line, e2b) = version.strip_prefix('v')?.split_once('-')?;
    Version::parse(e2b)
        .ok()
        .filter(|e2b| e2b.pre.is_empty() && e2b.build.is_empty())?;
    Version::parse(&format!("{line}.0")).ok()
}

fn strict_semver(text: &str) -> Option<Version> {
    Version::parse(text.strip_prefix('v').unwrap_or(text)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memfd_needs_release_line_1_14_in_every_format() {
        assert!(supports_memfd("v1.14.1_431f1fc"));
        assert!(supports_memfd("v1.14-0.1.0"));
        assert!(supports_memfd("v1.15.0"));
        assert!(supports_memfd("2.0.0"));
        assert!(supports_memfd("v1.14.0-rc.1_abc"));
        assert!(supports_memfd("v1.14.1_"));
        assert!(!supports_memfd("v1.12.1_a41d3fb"));
        assert!(!supports_memfd("v1.10-0.3.0"));
        assert!(!supports_memfd("v1.13.9"));
    }

    #[test]
    fn unparsable_versions_never_get_memfd() {
        assert!(!supports_memfd(""));
        assert!(!supports_memfd("latest"));
        assert!(!supports_memfd("v1.14"));
        assert!(!supports_memfd("v01.14.0"));
        assert!(!supports_memfd("v1.14-0.1"));
        assert!(!supports_memfd("v1.14.x_431f1fc"));
    }
}
