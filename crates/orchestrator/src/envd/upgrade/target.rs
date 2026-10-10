use std::path::{Path, PathBuf};

use super::super::version::is_at_least;

const OFF: &str = "off";
const PROMOTED: &str = "promoted";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    Stalled(String),
    Failed(String),
}

impl SourceError {
    pub fn is_stalled(&self) -> bool {
        matches!(self, SourceError::Stalled(_))
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Stalled(message) | SourceError::Failed(message) => f.write_str(message),
        }
    }
}

pub trait UpgradeSource {
    fn stat(&self, path: &Path) -> impl Future<Output = Result<(), SourceError>>;
    fn version(&self, path: &Path) -> impl Future<Output = Result<String, SourceError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decline {
    Off,
    NotStaged,
    InvalidTarget,
    GetVersionFailed,
    SameVersion,
    Downgrade,
    SourceStalled,
}

impl Decline {
    pub fn reason(self) -> &'static str {
        match self {
            Decline::Off => "off",
            Decline::NotStaged => "not_staged",
            Decline::InvalidTarget => "invalid_target",
            Decline::GetVersionFailed => "getversion_failed",
            Decline::SameVersion => "same_version",
            Decline::Downgrade => "downgrade",
            Decline::SourceStalled => "source_stalled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    pub path: PathBuf,
    pub version: String,
}

pub fn is_disabled(target: &str) -> bool {
    target.is_empty() || target == OFF
}

pub async fn candidate(source: &impl UpgradeSource, target: &str, host_envd: &Path) -> Result<PathBuf, Decline> {
    match target {
        target if is_disabled(target) => Err(Decline::Off),
        PROMOTED => promoted(source, host_envd).await,
        target => staged(source, target, host_envd).await,
    }
}

pub async fn resolve(
    source: &impl UpgradeSource,
    target: &str,
    built_with: &str,
    host_envd: &Path,
) -> Result<Upgrade, Decline> {
    let path = candidate(source, target, host_envd).await?;
    let version = match source.version(&path).await {
        Err(e) if e.is_stalled() => return Err(Decline::SourceStalled),
        Err(_) => return Err(Decline::GetVersionFailed),
        Ok(version) if version.is_empty() => return Err(Decline::GetVersionFailed),
        Ok(version) => version,
    };
    if version == built_with {
        return Err(Decline::SameVersion);
    }
    match is_at_least(&version, built_with) {
        Ok(true) => Ok(Upgrade { path, version }),
        _ => Err(Decline::Downgrade),
    }
}

async fn promoted(source: &impl UpgradeSource, host_envd: &Path) -> Result<PathBuf, Decline> {
    match source.stat(host_envd).await {
        Ok(()) => Ok(host_envd.to_path_buf()),
        Err(e) if e.is_stalled() => Err(Decline::SourceStalled),
        Err(_) => Err(Decline::NotStaged),
    }
}

async fn staged(source: &impl UpgradeSource, target: &str, host_envd: &Path) -> Result<PathBuf, Decline> {
    if !is_bare_identifier(target) {
        return Err(Decline::InvalidTarget);
    }
    let dir = host_envd.parent().unwrap_or(Path::new("."));
    for layout in [dir.join(format!("envd.{target}")), dir.join(target).join("envd")] {
        match source.stat(&layout).await {
            Ok(()) => return Ok(layout),
            Err(e) if e.is_stalled() => return Err(Decline::SourceStalled),
            Err(_) => {}
        }
    }
    Err(Decline::NotStaged)
}

fn is_bare_identifier(target: &str) -> bool {
    let mut characters = target.chars();
    characters.next().is_some_and(|first| first.is_ascii_alphanumeric())
        && characters.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    struct Staged {
        versions: HashMap<PathBuf, String>,
        stat_error: Option<SourceError>,
        version_error: Option<SourceError>,
        stats: Mutex<Vec<PathBuf>>,
        asked: Mutex<Vec<PathBuf>>,
    }

    impl Staged {
        fn new(versions: &[(&Path, &str)]) -> Self {
            Self {
                versions: versions
                    .iter()
                    .map(|(path, version)| (path.to_path_buf(), (*version).to_owned()))
                    .collect(),
                stat_error: None,
                version_error: None,
                stats: Mutex::default(),
                asked: Mutex::default(),
            }
        }
    }

    impl UpgradeSource for Staged {
        async fn stat(&self, path: &Path) -> Result<(), SourceError> {
            self.stats.lock().unwrap().push(path.to_path_buf());
            if let Some(error) = &self.stat_error {
                return Err(error.clone());
            }
            std::fs::metadata(path)
                .map(|_| ())
                .map_err(|e| SourceError::Failed(e.to_string()))
        }

        async fn version(&self, path: &Path) -> Result<String, SourceError> {
            self.asked.lock().unwrap().push(path.to_path_buf());
            if let Some(error) = &self.version_error {
                return Err(error.clone());
            }
            self.versions
                .get(path)
                .cloned()
                .ok_or_else(|| SourceError::Failed(format!("unknown binary {}", path.display())))
        }
    }

    struct Layout {
        _dir: tempfile::TempDir,
        promoted: PathBuf,
        sha: PathBuf,
        version: PathBuf,
        nested: PathBuf,
    }

    fn layout() -> Layout {
        let dir = tempfile::tempdir().unwrap();
        let promoted = dir.path().join("envd");
        let sha = dir.path().join("envd.1f95888");
        let version = dir.path().join("envd.v0.7.1");
        let nested = dir.path().join("v0.8.0").join("envd");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        [&promoted, &sha, &version, &nested]
            .iter()
            .for_each(|path| std::fs::write(path, "x").unwrap());
        Layout {
            _dir: dir,
            promoted,
            sha,
            version,
            nested,
        }
    }

    fn versions(layout: &Layout) -> Staged {
        Staged::new(&[
            (&layout.promoted, "0.6.12"),
            (&layout.sha, "0.7.0"),
            (&layout.version, "0.7.1"),
            (&layout.nested, "0.8.0"),
        ])
    }

    #[tokio::test]
    async fn the_target_flag_decides_which_binary_is_the_upgrade() {
        let layout = layout();
        let source = versions(&layout);
        let path = |binary: &Path, version: &str| {
            Ok(Upgrade {
                path: binary.to_path_buf(),
                version: version.to_owned(),
            })
        };
        for (name, target, built_with, expected) in [
            ("off returns empty", "off", "0.6.11", Err(Decline::Off)),
            ("empty (unset) returns empty", "", "0.6.11", Err(Decline::Off)),
            (
                "promoted, newer -> promoted path",
                "promoted",
                "0.6.11",
                path(&layout.promoted, "0.6.12"),
            ),
            (
                "promoted, same version -> empty (idempotent)",
                "promoted",
                "0.6.12",
                Err(Decline::SameVersion),
            ),
            (
                "sha, exists and newer -> sha path",
                "1f95888",
                "0.6.11",
                path(&layout.sha, "0.7.0"),
            ),
            (
                "sha, same version -> empty",
                "1f95888",
                "0.7.0",
                Err(Decline::SameVersion),
            ),
            (
                "sha, missing binary -> not_staged",
                "deadbee",
                "0.6.11",
                Err(Decline::NotStaged),
            ),
            (
                "version, exists and newer -> version path",
                "v0.7.1",
                "0.7.0",
                path(&layout.version, "0.7.1"),
            ),
            (
                "version, missing binary -> not_staged",
                "v0.9.9",
                "0.6.11",
                Err(Decline::NotStaged),
            ),
            (
                "sha, older target -> downgrade refused",
                "1f95888",
                "0.8.0",
                Err(Decline::Downgrade),
            ),
            (
                "promoted, older target -> downgrade refused",
                "promoted",
                "0.7.0",
                Err(Decline::Downgrade),
            ),
            (
                "target with .. -> invalid_target",
                "../../etc/passwd",
                "0.6.11",
                Err(Decline::InvalidTarget),
            ),
            (
                "target with slash -> invalid_target",
                "sub/envd",
                "0.6.11",
                Err(Decline::InvalidTarget),
            ),
            (
                "target with leading dot -> invalid_target",
                ".hidden",
                "0.6.11",
                Err(Decline::InvalidTarget),
            ),
            (
                "target starting with dot -> invalid_target",
                ".v0.7.0",
                "0.6.11",
                Err(Decline::InvalidTarget),
            ),
            (
                "release target, nested layout -> nested path",
                "v0.8.0",
                "0.6.11",
                path(&layout.nested, "0.8.0"),
            ),
            (
                "release target, not staged",
                "v9.9.9",
                "0.6.11",
                Err(Decline::NotStaged),
            ),
            (
                "dashed release target, not staged",
                "v0.7.0-rc1",
                "0.6.11",
                Err(Decline::NotStaged),
            ),
        ] {
            assert_eq!(
                resolve(&source, target, built_with, &layout.promoted).await,
                expected,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn an_unreadable_target_is_no_upgrade_and_not_an_error() {
        let layout = layout();
        let mut source = versions(&layout);
        source.version_error = Some(SourceError::Failed("boom".to_owned()));

        let resolved = resolve(&source, "promoted", "0.6.11", &layout.promoted).await;

        assert_eq!(resolved, Err(Decline::GetVersionFailed));
    }

    #[tokio::test]
    async fn a_target_that_names_no_version_is_unreadable() {
        let layout = layout();
        let source = Staged::new(&[(&layout.promoted, "")]);

        let resolved = resolve(&source, "promoted", "0.6.11", &layout.promoted).await;

        assert_eq!(resolved, Err(Decline::GetVersionFailed));
    }

    #[tokio::test]
    async fn the_version_is_probed_exactly_once_on_the_path_the_upgrade_returns() {
        let layout = layout();
        let source = Staged::new(&[(&layout.promoted, "0.7.0")]);

        let upgrade = resolve(&source, "promoted", "0.6.0", &layout.promoted).await.unwrap();

        let asked = source.asked.lock().unwrap().clone();
        assert_eq!(
            asked.len(),
            1,
            "exactly one probe, or the delivery refusal becomes reachable"
        );
        assert_eq!(asked[0], upgrade.path);
    }

    #[tokio::test]
    async fn a_candidate_stat_that_stalls_is_source_stalled_not_not_staged() {
        let layout = layout();
        let mut source = versions(&layout);
        source.stat_error = Some(SourceError::Stalled("stat: stalled".to_owned()));

        for target in ["promoted", "v0.9.0"] {
            let resolved = resolve(&source, target, "0.6.0", &layout.promoted).await;
            assert_eq!(resolved, Err(Decline::SourceStalled), "{target}");
        }
        assert!(
            source.asked.lock().unwrap().is_empty(),
            "a stalled check must not probe the binary"
        );
    }

    #[tokio::test]
    async fn a_probe_that_stalls_is_source_stalled() {
        let layout = layout();
        let mut source = versions(&layout);
        source.version_error = Some(SourceError::Stalled("lookup: stalled".to_owned()));

        let resolved = resolve(&source, "promoted", "0.6.0", &layout.promoted).await;

        assert_eq!(resolved, Err(Decline::SourceStalled));
    }

    #[tokio::test]
    async fn a_candidate_that_is_missing_is_still_not_staged() {
        let layout = layout();
        let mut source = versions(&layout);
        source.stat_error = Some(SourceError::Failed("no such file".to_owned()));

        for target in ["promoted", "v0.9.0"] {
            let resolved = resolve(&source, target, "0.6.0", &layout.promoted).await;
            assert_eq!(resolved, Err(Decline::NotStaged), "{target}");
        }
        assert!(source.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_version_pinned_candidate_is_not_stat_ed_twice() {
        let layout = layout();
        let flat = layout.promoted.parent().unwrap().join("envd.v0.9.0");
        std::fs::write(&flat, "x").unwrap();
        let source = versions(&layout);

        let found = candidate(&source, "v0.9.0", &layout.promoted).await;

        assert_eq!(found, Ok(flat.clone()));
        assert_eq!(*source.stats.lock().unwrap(), vec![flat]);
    }

    #[tokio::test]
    async fn the_offline_convergence_refires_until_the_built_with_version_advances() {
        let layout = layout();
        let source = Staged::new(&[(&layout.promoted, "0.6.12")]);

        let first = resolve(&source, "promoted", "0.6.5", &layout.promoted).await;
        let again = resolve(&source, "promoted", "0.6.5", &layout.promoted).await;
        let converged = resolve(&source, "promoted", "0.6.12", &layout.promoted).await;

        assert_eq!(first, again, "built-with never advances, so the swap re-fires");
        assert!(first.is_ok());
        assert_eq!(converged, Err(Decline::SameVersion));
    }

    #[test]
    fn only_empty_and_off_disable_the_target() {
        assert!(is_disabled(""));
        assert!(is_disabled("off"));
        assert!(!is_disabled("promoted"));
        assert!(!is_disabled("v0.7.0"));
    }

    #[test]
    fn the_reasons_read_like_upstream() {
        let reasons = [
            Decline::Off,
            Decline::NotStaged,
            Decline::InvalidTarget,
            Decline::GetVersionFailed,
            Decline::SameVersion,
            Decline::Downgrade,
            Decline::SourceStalled,
        ]
        .map(Decline::reason);
        assert_eq!(
            reasons,
            [
                "off",
                "not_staged",
                "invalid_target",
                "getversion_failed",
                "same_version",
                "downgrade",
                "source_stalled"
            ]
        );
    }
}
