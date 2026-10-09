use std::path::{Path, PathBuf};

use crate::go_exec::GoCommand;
use crate::go_strconv::quote;

const PROMOTED: &str = "promoted";

pub async fn envd_version(envd: &Path) -> Result<String, String> {
    GoCommand::new(envd)
        .args(["-version"])
        .run()
        .await
        .map(|output| String::from_utf8_lossy(&output).trim().to_owned())
        .map_err(|e| format!("error while getting envd version: {}", e.message))
}

pub async fn resolve_build_binary(target: &str, host_envd: &Path) -> Result<PathBuf, String> {
    if target.is_empty() || target == PROMOTED {
        return Ok(host_envd.to_path_buf());
    }
    if !is_version_like(target) {
        return Err(format!("invalid build-envd-version target {}", quote(target)));
    }
    let dir = host_envd.parent().unwrap_or(Path::new("."));
    let candidates = [dir.join(format!("envd.{target}")), dir.join(target).join("envd")];
    if let Some(staged) = candidates.iter().find(|candidate| candidate.exists()) {
        return Ok(staged.clone());
    }
    let tried = format!(
        "[{}]",
        candidates
            .iter()
            .map(|candidate| candidate.display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let promoted = envd_version(host_envd).await.map_err(|e| {
        format!(
            "build-envd-version target {} is not staged under {} (tried {tried}), and the promoted binary's version is unreadable: {e}",
            quote(target),
            quote(&dir.display().to_string()),
        )
    })?;
    match promoted.trim_start_matches('v') == target.trim_start_matches('v') {
        true => Ok(host_envd.to_path_buf()),
        false => Err(format!(
            "build-envd-version target {} is not staged under {} (tried {tried}), and the promoted binary is v{}, not the target",
            quote(target),
            quote(&dir.display().to_string()),
            promoted.trim_start_matches('v'),
        )),
    }
}

fn is_version_like(target: &str) -> bool {
    let mut chars = target.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn script(dir: &Path, name: &str, version: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho {version}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn promoted_and_empty_keep_the_host_binary() {
        let host = Path::new("/fc-envd/envd");

        assert_eq!(resolve_build_binary("promoted", host).await.unwrap(), host);
        assert_eq!(resolve_build_binary("", host).await.unwrap(), host);
    }

    #[tokio::test]
    async fn targets_must_look_like_versions() {
        for target in ["../x", ".hidden", "a/b"] {
            assert_eq!(
                resolve_build_binary(target, Path::new("/e/envd")).await.unwrap_err(),
                format!("invalid build-envd-version target \"{target}\"")
            );
        }
    }

    #[tokio::test]
    async fn a_staged_sibling_wins_in_both_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let host = script(dir.path(), "envd", "0.9.0");
        let flat = script(dir.path(), "envd.v0.7.0", "0.7.0");
        std::fs::create_dir(dir.path().join("v0.8.0")).unwrap();
        let nested = script(&dir.path().join("v0.8.0"), "envd", "0.8.0");

        assert_eq!(resolve_build_binary("v0.7.0", &host).await.unwrap(), flat);
        assert_eq!(resolve_build_binary("v0.8.0", &host).await.unwrap(), nested);
    }

    #[tokio::test]
    async fn the_promoted_binary_serves_its_own_version_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let host = script(dir.path(), "envd", "0.9.0");

        assert_eq!(resolve_build_binary("v0.9.0", &host).await.unwrap(), host);
        let error = resolve_build_binary("0.7.0", &host).await.unwrap_err();
        assert!(
            error.ends_with("and the promoted binary is v0.9.0, not the target"),
            "{error}"
        );
    }
}
