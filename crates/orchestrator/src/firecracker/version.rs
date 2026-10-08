const MEMFD_RELEASE: Release = Release { major: 1, minor: 14 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Release {
    major: u64,
    minor: u64,
}

pub fn supports_memfd(firecracker_version: &str) -> bool {
    release_line(firecracker_version).is_some_and(|release| release >= MEMFD_RELEASE)
}

fn release_line(version: &str) -> Option<Release> {
    match version.split_once('_') {
        Some((release, _)) => strict_semver(release.strip_prefix('v').unwrap_or(release)),
        None => e2b_release(version).or_else(|| strict_semver(version.strip_prefix('v').unwrap_or(version))),
    }
}

fn e2b_release(version: &str) -> Option<Release> {
    let (line, e2b) = version.strip_prefix('v')?.split_once('-')?;
    let [major, minor] = digits_parts(line)?;
    let [_, _, _] = digits_parts::<3>(e2b)?;
    strict_semver(&format!("{major}.{minor}.0"))?;
    strict_semver(e2b)?;
    Some(Release {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
    })
}

fn digits_parts<const N: usize>(text: &str) -> Option<[&str; N]> {
    let parts: Vec<&str> = text.split('.').collect();
    parts
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(())?;
    parts.try_into().ok()
}

fn strict_semver(text: &str) -> Option<Release> {
    let (rest, metadata) = text
        .split_once('+')
        .map_or((text, None), |(rest, metadata)| (rest, Some(metadata)));
    let (core, prerelease) = rest
        .split_once('-')
        .map_or((rest, None), |(core, prerelease)| (core, Some(prerelease)));
    let [major, minor, patch] = digits_parts(core)?;
    let numbers_valid = [major, minor, patch].iter().all(|number| no_leading_zero(number));
    let prerelease_valid = prerelease.is_none_or(|prerelease| identifiers_valid(prerelease, true));
    let metadata_valid = metadata.is_none_or(|metadata| identifiers_valid(metadata, false));
    (numbers_valid && prerelease_valid && metadata_valid).then_some(Release {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
    })
}

fn no_leading_zero(number: &str) -> bool {
    number == "0" || !number.starts_with('0')
}

fn identifiers_valid(text: &str, numeric_without_leading_zero: bool) -> bool {
    text.split('.').all(|identifier| {
        !identifier.is_empty()
            && identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && (!numeric_without_leading_zero
                || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                || no_leading_zero(identifier))
    })
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
