use std::path::{Path, PathBuf};

const DATA_DIRS: [&str; 6] = ["kernels", "templates", "sandbox", "orchestrator", "fc-versions", "envd"];
const ORCHESTRATOR_DIRS: [&str; 4] = ["build", "build-templates", "sandbox", "template"];
const REMOTE_DATA_DIR: &str = ".local-build";

pub struct Setup<'a> {
    pub storage: &'a str,
    pub sandbox_dir: &'a str,
    pub storage_explicit: bool,
}

pub fn setup_env(setup: &Setup) -> Result<(), String> {
    if !setup.sandbox_dir.is_empty() {
        set("SANDBOX_DIR", setup.sandbox_dir);
    }
    let data_dir = match setup.storage.starts_with("gs://") {
        true => PathBuf::from(REMOTE_DATA_DIR),
        false => PathBuf::from(setup.storage),
    };
    DATA_DIRS
        .iter()
        .map(|dir| data_dir.join(dir))
        .chain(
            ORCHESTRATOR_DIRS
                .iter()
                .map(|dir| data_dir.join("orchestrator").join(dir)),
        )
        .try_for_each(|dir| std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display())))?;
    [
        ("ARTIFACTS_REGISTRY_PROVIDER", "Local".to_owned()),
        ("FIRECRACKER_VERSIONS_DIR", absolute(&data_dir.join("fc-versions"))),
        ("HOST_ENVD_PATH", absolute(&data_dir.join("envd").join("envd"))),
        ("HOST_KERNELS_DIR", absolute(&data_dir.join("kernels"))),
        ("ORCHESTRATOR_BASE_PATH", absolute(&data_dir.join("orchestrator"))),
    ]
    .iter()
    .filter(|(key, _)| std::env::var(key).unwrap_or_default().is_empty())
    .for_each(|(key, value)| set(key, value));
    let template_url = match setup.storage.starts_with("gs://") {
        true => setup.storage.to_owned(),
        false => format!("file://{}", absolute(&data_dir.join("templates"))),
    };
    let unconfigured = ["TEMPLATE_STORAGE_URL", "STORAGE_PROVIDER"]
        .iter()
        .all(|key| std::env::var(key).unwrap_or_default().is_empty());
    if setup.storage_explicit || unconfigured {
        set("TEMPLATE_STORAGE_URL", &template_url);
    }
    if std::env::var("NODE_IP").unwrap_or_default().is_empty() {
        set("NODE_IP", "127.0.0.1");
    }
    Ok(())
}

fn absolute(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn set(key: &str, value: &str) {
    unsafe { std::env::set_var(key, value) };
}
