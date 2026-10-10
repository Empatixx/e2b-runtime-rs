use semver::Version;

pub fn supports_memfd(firecracker_version: &str) -> bool {
    release_line(firecracker_version).is_some_and(|release| (release.major, release.minor) >= (1, 14))
}

pub fn supports_huge_pages(firecracker_version: &str) -> bool {
    release_line(firecracker_version).is_some_and(|release| (release.major, release.minor) >= (1, 7))
}

pub fn supports_free_page_reporting(firecracker_version: &str) -> bool {
    supports_memfd(firecracker_version)
}

pub fn supports_free_page_hinting(firecracker_version: &str) -> bool {
    supports_memfd(firecracker_version)
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

pub fn supports_in_place_checkpoint(firecracker_version: &str) -> bool {
    e2b_release_version(firecracker_version).is_some_and(|release| release >= Version::new(0, 2, 0))
}

pub fn supports_filesystem_snapshots(firecracker_version: &str) -> bool {
    e2b_release_version(firecracker_version).is_some_and(|release| release >= Version::new(0, 1, 0))
}

fn e2b_release_version(version: &str) -> Option<Version> {
    let (line, release) = version.strip_prefix('v')?.split_once('-')?;
    let is_line = line
        .split_once('.')
        .is_some_and(|(major, minor)| digits(major) && digits(minor));
    let release = is_line.then(|| Version::parse(release).ok()).flatten()?;
    (release.pre.is_empty() && release.build.is_empty()).then_some(release)
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
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
    fn the_balloon_features_need_release_line_1_14() {
        for version in ["v1.14-0.2.0", "v1.14.1_621718e", "v1.15.0"] {
            assert!(supports_free_page_reporting(version), "{version}");
            assert!(supports_free_page_hinting(version), "{version}");
        }
        for version in ["v1.12.1_a41d3fb", "v1.10-0.3.0", "latest"] {
            assert!(!supports_free_page_reporting(version), "{version}");
            assert!(!supports_free_page_hinting(version), "{version}");
        }
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
    fn the_in_place_checkpoint_needs_e2b_release_0_2_0_and_filesystem_snapshots_0_1_0() {
        let cases = [
            ("v1.14-0.2.0", true, true),
            ("v1.14-0.2.1", true, true),
            ("v1.14-0.3.0", true, true),
            ("v1.14-1.0.0", true, true),
            ("v1.15-0.2.0", true, true),
            ("v1.14-0.1.0", false, true),
            ("v1.14-0.1.1", false, true),
            ("v1.14.1_431f1fc", false, false),
            ("v1.14.1_6ecb627", false, false),
            ("v1.10.1", false, false),
        ];

        for (version, in_place, filesystem) in cases {
            assert_eq!(supports_in_place_checkpoint(version), in_place, "in place {version}");
            assert_eq!(
                supports_filesystem_snapshots(version),
                filesystem,
                "filesystem {version}"
            );
        }
    }

    #[test]
    fn unparsable_versions_never_get_the_e2b_features() {
        for version in ["", "latest", "v1.14-0.2", "v1.14-0.2.0-rc.1", "v1-0.2.0", "v1.x-0.2.0"] {
            assert!(!supports_in_place_checkpoint(version), "{version}");
            assert!(!supports_filesystem_snapshots(version), "{version}");
        }
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
