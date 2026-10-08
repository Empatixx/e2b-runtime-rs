use std::cmp::Ordering;

const MIN_FOR_SNAPSHOT: &str = "0.5.0";

pub fn check_for_snapshot(envd_version: &str) -> Result<(), String> {
    is_at_least(envd_version, MIN_FOR_SNAPSHOT)
        .map_err(|e| format!("invalid envd version {envd_version:?}: {e}"))?
        .then_some(())
        .ok_or_else(|| {
            format!(
                "sandbox envd version must be at least {MIN_FOR_SNAPSHOT} to create snapshots, current version: {envd_version}"
            )
        })
}

fn is_at_least(current: &str, minimum: &str) -> Result<bool, String> {
    let current = sanitized(current);
    let minimum = sanitized(minimum);
    let current = Version::parse(&current).ok_or_else(|| format!("invalid current version format: {current}"))?;
    let minimum = Version::parse(&minimum).ok_or_else(|| format!("invalid minimum version format: {minimum}"))?;
    Ok(current.compare(&minimum) != Ordering::Less)
}

fn sanitized(version: &str) -> String {
    match version.starts_with('v') || version.is_empty() {
        true => version.to_owned(),
        false => format!("v{version}"),
    }
}

struct Version<'a> {
    numbers: [&'a str; 3],
    prerelease: Option<&'a str>,
}

impl<'a> Version<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let rest = text.strip_prefix('v')?;
        let (rest, build) = rest
            .split_once('+')
            .map_or((rest, None), |(rest, build)| (rest, Some(build)));
        let (core, prerelease) = rest
            .split_once('-')
            .map_or((rest, None), |(core, prerelease)| (core, Some(prerelease)));
        let parts: Vec<&str> = core.split('.').collect();
        let full = parts.len() == 3;
        let valid = (1..=3).contains(&parts.len())
            && parts.iter().all(|part| is_number(part))
            && (full || (prerelease.is_none() && build.is_none()))
            && prerelease.is_none_or(|prerelease| identifiers_valid(prerelease, true))
            && build.is_none_or(|build| identifiers_valid(build, false));
        valid.then(|| Version {
            numbers: [0, 1, 2].map(|index| parts.get(index).copied().unwrap_or("0")),
            prerelease,
        })
    }

    fn compare(&self, other: &Version) -> Ordering {
        self.numbers
            .iter()
            .zip(other.numbers)
            .map(|(left, right)| compare_numbers(left, right))
            .find(|ordering| ordering.is_ne())
            .unwrap_or_else(|| compare_prereleases(self.prerelease, other.prerelease))
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) && (text == "0" || !text.starts_with('0'))
}

fn identifiers_valid(text: &str, numeric_without_leading_zero: bool) -> bool {
    text.split('.').all(|identifier| {
        !identifier.is_empty()
            && identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && (!numeric_without_leading_zero
                || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                || is_number(identifier))
    })
}

fn compare_numbers(left: &str, right: &str) -> Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn compare_prereleases(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => left
            .split('.')
            .zip(right.split('.'))
            .map(|(left, right)| compare_identifiers(left, right))
            .find(|ordering| ordering.is_ne())
            .unwrap_or_else(|| left.split('.').count().cmp(&right.split('.').count())),
    }
}

fn compare_identifiers(left: &str, right: &str) -> Ordering {
    let numeric = |identifier: &str| identifier.bytes().all(|byte| byte.is_ascii_digit());
    match (numeric(left), numeric(right)) {
        (true, true) => compare_numbers(left, right),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left.cmp(right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_from_0_5_0_can_snapshot() {
        assert_eq!(check_for_snapshot("0.5.0"), Ok(()));
        assert_eq!(check_for_snapshot("v0.6.4"), Ok(()));
        assert_eq!(check_for_snapshot("1"), Ok(()));
        assert_eq!(check_for_snapshot("0.10.0"), Ok(()));
    }

    #[test]
    fn older_versions_are_refused_with_the_upstream_text() {
        assert_eq!(
            check_for_snapshot("0.4.9"),
            Err("sandbox envd version must be at least 0.5.0 to create snapshots, current version: 0.4.9".to_owned())
        );
        assert_eq!(
            check_for_snapshot("0.5.0-rc.1"),
            Err(
                "sandbox envd version must be at least 0.5.0 to create snapshots, current version: 0.5.0-rc.1"
                    .to_owned()
            )
        );
    }

    #[test]
    fn malformed_versions_are_reported() {
        assert_eq!(
            check_for_snapshot(""),
            Err("invalid envd version \"\": invalid current version format: ".to_owned())
        );
        assert_eq!(
            check_for_snapshot("0.5-rc"),
            Err("invalid envd version \"0.5-rc\": invalid current version format: v0.5-rc".to_owned())
        );
        assert_eq!(
            check_for_snapshot("01.2.3"),
            Err("invalid envd version \"01.2.3\": invalid current version format: v01.2.3".to_owned())
        );
    }

    #[test]
    fn prereleases_order_like_go_semver() {
        let order = [
            "v1.0.0-alpha",
            "v1.0.0-alpha.1",
            "v1.0.0-alpha.beta",
            "v1.0.0-beta.2",
            "v1.0.0-beta.11",
            "v1.0.0",
        ];
        order.windows(2).for_each(|pair| {
            let left = Version::parse(pair[0]).unwrap();
            let right = Version::parse(pair[1]).unwrap();
            assert_eq!(left.compare(&right), Ordering::Less, "{} < {}", pair[0], pair[1]);
        });
        let release = Version::parse("v1.2.0").unwrap();
        assert_eq!(Version::parse("v1.2").unwrap().compare(&release), Ordering::Equal);
        assert_eq!(
            Version::parse("v1.2.0+build").unwrap().compare(&release),
            Ordering::Equal
        );
        assert!(Version::parse("v1.2+build").is_none());
    }
}
