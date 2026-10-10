use semver::Version;

pub fn supports_memfd(firecracker_version: &str) -> bool {
    release_line(firecracker_version).is_some_and(|release| (release.major, release.minor) >= (1, 14))
}

pub fn supports_huge_pages(firecracker_version: &str) -> bool {
    release_line(firecracker_version).is_some_and(|release| (release.major, release.minor) >= (1, 7))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirecrackerVersion {
    e2b_release: Option<Version>,
}

const TSC_KHZ_TEMPLATE_MIN_E2B: Version = Version::new(0, 3, 0);

impl FirecrackerVersion {
    pub fn has_tsc_khz_template(&self) -> bool {
        self.e2b_release
            .as_ref()
            .is_some_and(|release| *release >= TSC_KHZ_TEMPLATE_MIN_E2B)
    }
}

pub fn parse_firecracker_version(version: &str) -> Result<FirecrackerVersion, String> {
    let invalid = || {
        format!(
            "invalid firecracker version {}: Invalid Semantic Version",
            crate::go_strconv::quote(version)
        )
    };
    match version.split_once('_') {
        Some((release, _)) => strict_semver(release)
            .map(|_| FirecrackerVersion { e2b_release: None })
            .ok_or_else(invalid),
        None => match strict_e2b(version) {
            Some(e2b_release) => Ok(FirecrackerVersion {
                e2b_release: Some(e2b_release),
            }),
            None => strict_semver(version)
                .map(|_| FirecrackerVersion { e2b_release: None })
                .ok_or_else(invalid),
        },
    }
}

fn strict_e2b(version: &str) -> Option<Version> {
    let (line, e2b) = version.strip_prefix('v')?.split_once('-')?;
    let digits = |text: &str, parts: usize| {
        let fields: Vec<&str> = text.split('.').collect();
        fields.len() == parts
            && fields
                .iter()
                .all(|field| !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit()))
    };
    (digits(line, 2) && digits(e2b, 3)).then_some(())?;
    Version::parse(&format!("{line}.0")).ok()?;
    Version::parse(e2b).ok()
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
    fn huge_pages_need_release_line_1_7_in_every_format() {
        assert!(supports_huge_pages("v1.14.1_431f1fc"));
        assert!(supports_huge_pages("v1.14-0.2.0"));
        assert!(supports_huge_pages("v1.7.0"));
        assert!(supports_huge_pages("v1.10-0.3.0"));
        assert!(supports_huge_pages("2.0.0"));
        assert!(!supports_huge_pages("v1.6.0"));
        assert!(!supports_huge_pages("v1.5.1_abc"));
        assert!(!supports_huge_pages(""));
        assert!(!supports_huge_pages("latest"));
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

    #[test]
    fn the_tsc_frequency_template_needs_an_e2b_release_from_0_3_0() {
        for (version, want) in [
            ("v1.14-0.3.0", true),
            ("v1.14-0.3.1", true),
            ("v1.14-1.0.0", true),
            ("v1.14-0.2.0", false),
            ("v1.14.1_431f1fc", false),
            ("v1.10.1", false),
        ] {
            assert_eq!(
                parse_firecracker_version(version).unwrap().has_tsc_khz_template(),
                want,
                "{version}"
            );
        }
    }

    #[test]
    fn an_unknown_format_is_an_invalid_version() {
        assert_eq!(
            parse_firecracker_version("not-a-version"),
            Err("invalid firecracker version \"not-a-version\": Invalid Semantic Version".to_owned())
        );
    }
}
