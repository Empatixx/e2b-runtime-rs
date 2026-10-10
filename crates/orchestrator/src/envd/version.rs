use std::cmp::Ordering;

use semver::Version;

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

pub fn is_at_least(current: &str, minimum: &str) -> Result<bool, String> {
    let current = sanitized(current);
    let minimum = sanitized(minimum);
    let current = go_semver(&current).ok_or_else(|| format!("invalid current version format: {current}"))?;
    let minimum = go_semver(&minimum).ok_or_else(|| format!("invalid minimum version format: {minimum}"))?;
    Ok(current.cmp_precedence(&minimum) != Ordering::Less)
}

fn sanitized(version: &str) -> String {
    match version.starts_with('v') || version.is_empty() {
        true => version.to_owned(),
        false => format!("v{version}"),
    }
}

fn go_semver(text: &str) -> Option<Version> {
    let rest = text.strip_prefix('v')?;
    let padding = match rest.split('.').count() {
        1 => ".0.0",
        2 => ".0",
        _ => "",
    };
    (padding.is_empty() || !rest.contains(['-', '+'])).then_some(())?;
    Version::parse(&format!("{rest}{padding}")).ok()
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
            let left = go_semver(pair[0]).unwrap();
            let right = go_semver(pair[1]).unwrap();
            assert_eq!(left.cmp_precedence(&right), Ordering::Less, "{} < {}", pair[0], pair[1]);
        });
        let release = go_semver("v1.2.0").unwrap();
        assert_eq!(go_semver("v1.2").unwrap().cmp_precedence(&release), Ordering::Equal);
        assert_eq!(
            go_semver("v1.2.0+build").unwrap().cmp_precedence(&release),
            Ordering::Equal
        );
        assert!(go_semver("v1.2+build").is_none());
    }
}
