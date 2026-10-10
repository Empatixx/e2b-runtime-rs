const SPEC_INTERNAL_PATHS: [&str; 6] = ["/collapse", "/freeze", "/fsfreeze", "/fsthaw", "/init", "/unfreeze"];
const UNSPECIFIED_INTERNAL_PATHS: [&str; 1] = ["/upgrade"];

pub fn is_internal_path(request_path: &str) -> bool {
    Some(request_path)
        .filter(|path| !path.is_empty())
        .map(clean_rooted)
        .is_some_and(|cleaned| {
            SPEC_INTERNAL_PATHS
                .iter()
                .chain(UNSPECIFIED_INTERNAL_PATHS.iter())
                .any(|internal| *internal == cleaned)
        })
}

fn clean_rooted(path: &str) -> String {
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .fold(Vec::new(), |mut kept, segment| {
            match segment {
                ".." => drop(kept.pop()),
                other => kept.push(other),
            }
            kept
        });
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = include_str!("../../../../envd/reference/envd.yaml");

    fn spec_paths() -> Vec<(String, bool)> {
        let mut paths: Vec<(String, bool)> = Vec::new();
        for line in SPEC.lines() {
            match line.strip_prefix("  /").and_then(|rest| rest.strip_suffix(':')) {
                Some(path) => paths.push((format!("/{path}"), false)),
                None if line.trim() == "x-internal: true" => {
                    if let Some(last) = paths.last_mut() {
                        last.1 = true;
                    }
                }
                None => {}
            }
        }
        paths
    }

    #[test]
    fn spec_internal_paths_match_the_reference_spec() {
        let mut want = spec_paths()
            .into_iter()
            .filter(|(_, internal)| *internal)
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        want.sort();
        assert_eq!(want, SPEC_INTERNAL_PATHS);
    }

    #[test]
    fn unspecified_internal_paths_are_absent_from_the_spec() {
        let paths = spec_paths();
        for path in UNSPECIFIED_INTERNAL_PATHS {
            assert!(paths.iter().all(|(spec_path, _)| spec_path != path), "{path}");
        }
    }

    #[test]
    fn refuses_the_control_routes() {
        for path in [
            "/init",
            "/freeze",
            "/unfreeze",
            "/collapse",
            "/fsfreeze",
            "/fsthaw",
            "/upgrade",
        ] {
            assert!(is_internal_path(path), "{path}");
        }
    }

    #[test]
    fn refuses_spellings_envd_would_have_404ed() {
        for path in ["/init/", "//init", "/./init", "/files/../init", "init"] {
            assert!(is_internal_path(path), "{path}");
        }
    }

    #[test]
    fn leaves_the_public_surface_alone() {
        for path in [
            "/health",
            "/metrics",
            "/envs",
            "/files",
            "/files/compose",
            "/nope",
            "/initialize",
            "/init/nested",
            "",
            "/INIT",
            "/%69nit",
        ] {
            assert!(!is_internal_path(path), "{path}");
        }
    }
}
