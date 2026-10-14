use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::util::fs::os_error;

const CA_BUNDLE_PATH: &str = "/etc/ssl/certs/ca-certificates.crt";
const CA_EXTRA_PATH: &str = "/usr/local/share/ca-certificates/e2b-ca.crt";

pub struct CaCertInstaller {
    bundle: PathBuf,
    extra: PathBuf,
    last: Arc<Mutex<String>>,
}

impl Default for CaCertInstaller {
    fn default() -> Self {
        Self::at(PathBuf::from(CA_BUNDLE_PATH), PathBuf::from(CA_EXTRA_PATH))
    }
}

impl CaCertInstaller {
    fn at(bundle: PathBuf, extra: PathBuf) -> Self {
        Self {
            bundle,
            extra,
            last: Arc::default(),
        }
    }

    pub async fn install(&self, cert_pem: &str) -> Result<(), String> {
        if cert_pem.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        let normalized = format!("{}\n", cert_pem.trim_end_matches('\n'));
        let mut last = self.last.lock().await;
        if *last == normalized {
            tracing::debug!(duration = ?started.elapsed(), "CA cert unchanged, skipping install");
            return Ok(());
        }
        let previous = last.clone();
        append(&self.bundle, &normalized)?;
        *last = normalized.clone();
        drop(last);
        tracing::info!(append_duration = ?started.elapsed(), "CA cert appended to bundle");
        tokio::spawn(clean_up(
            self.last.clone(),
            self.bundle.clone(),
            self.extra.clone(),
            normalized,
            previous,
        ));
        Ok(())
    }
}

fn append(bundle: &Path, cert: &str) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(bundle)
        .map_err(|e| format!("open CA bundle: {}", os_error("open", bundle, &e)))?;
    file.write_all(cert.as_bytes())
        .map_err(|e| format!("append CA cert: {}", os_error("write", bundle, &e)))
}

async fn clean_up(last: Arc<Mutex<String>>, bundle: PathBuf, extra: PathBuf, normalized: String, previous: String) {
    let started = Instant::now();
    let last = last.lock().await;
    if *last != normalized {
        return;
    }
    let written = extra
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(|e| ("Failed to create CA extra dir", e))
        .and_then(|()| std::fs::write(&extra, &normalized).map_err(|e| ("Failed to write CA cert to extra dir", e)));
    if let Err((message, e)) = written {
        tracing::error!(error = %e, "{message}");
    }
    if previous.is_empty() {
        return;
    }
    match remove_cert(&bundle, &previous) {
        Ok(()) => tracing::info!(cleanup_duration = ?started.elapsed(), "Old CA cert removed from bundle"),
        Err(e) => tracing::error!(error = %e, "Failed to remove old CA cert from bundle"),
    }
}

fn remove_cert(bundle: &Path, cert: &str) -> Result<(), String> {
    let content = std::fs::read_to_string(bundle).map_err(|e| os_error("open", bundle, &e))?;
    let directory = bundle.parent().unwrap_or(Path::new("/"));
    let mut temporary = tempfile::Builder::new()
        .prefix("ca-bundle-")
        .tempfile_in(directory)
        .map_err(|e| format!("create temp file: {}", crate::util::errno::message(&e)))?;
    temporary
        .write_all(content.replace(cert, "").as_bytes())
        .map_err(|e| format!("write temp file: {}", crate::util::errno::message(&e)))?;
    set_mode(temporary.path())?;
    temporary
        .persist(bundle)
        .map(|_| ())
        .map_err(|e| format!("rename temp file: {}", crate::util::errno::message(&e.error)))
}

fn set_mode(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("chmod temp file: {}", crate::util::errno::message(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_new_bundle_is_appended_and_replaces_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("ca-certificates.crt");
        let extra = dir.path().join("extra/e2b-ca.crt");
        std::fs::write(&bundle, "SYSTEM\n").unwrap();
        let installer = CaCertInstaller::at(bundle.clone(), extra.clone());

        installer.install("FIRST\n\n").await.unwrap();
        installer.install("FIRST").await.unwrap();
        installer.install("SECOND").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(std::fs::read_to_string(&bundle).unwrap(), "SYSTEM\nSECOND\n");
        assert_eq!(std::fs::read_to_string(&extra).unwrap(), "SECOND\n");
        let missing = CaCertInstaller::at(dir.path().join("none"), extra);
        assert_eq!(
            missing.install("X").await.unwrap_err(),
            format!(
                "open CA bundle: open {}: no such file or directory",
                dir.path().join("none").display()
            )
        );
    }
}
