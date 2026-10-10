use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEFAULT_FIRECRACKER_VERSIONS_DIR: &str = "/fc-versions";
const DEFAULT_KERNELS_DIR: &str = "/fc-kernels";
const FIRECRACKER_BINARY: &str = "firecracker";
const KERNEL_FILE: &str = "vmlinux.bin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binaries {
    firecracker: Option<PathBuf>,
    kernel: Option<PathBuf>,
    firecracker_versions_dir: PathBuf,
    kernels_dir: PathBuf,
    arch: String,
}

impl Binaries {
    pub fn from_vars(vars: &HashMap<String, String>, arch: &str, firecracker: &Path, kernel: &Path) -> Self {
        let dir = |name: &str, default: &str| {
            vars.get(name)
                .filter(|value| !value.is_empty())
                .map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        Self {
            firecracker: Some(firecracker)
                .filter(|path| !path.as_os_str().is_empty())
                .map(Path::to_path_buf),
            kernel: Some(kernel)
                .filter(|path| !path.as_os_str().is_empty())
                .map(Path::to_path_buf),
            firecracker_versions_dir: dir("FIRECRACKER_VERSIONS_DIR", DEFAULT_FIRECRACKER_VERSIONS_DIR),
            kernels_dir: dir("HOST_KERNELS_DIR", DEFAULT_KERNELS_DIR),
            arch: arch.to_owned(),
        }
    }

    pub fn firecracker_versions_dir(&self) -> &Path {
        &self.firecracker_versions_dir
    }

    pub fn kernels_dir(&self) -> &Path {
        &self.kernels_dir
    }

    pub fn arch(&self) -> &str {
        &self.arch
    }

    pub fn firecracker_for(&self, version: &str) -> PathBuf {
        self.firecracker.clone().unwrap_or_else(|| {
            let base = self.firecracker_versions_dir.join(version);
            prefer_arch_layout(
                base.join(&self.arch).join(FIRECRACKER_BINARY),
                base.join(FIRECRACKER_BINARY),
            )
        })
    }

    pub fn kernel_for(&self, version: &str) -> PathBuf {
        self.kernel.clone().unwrap_or_else(|| {
            let base = self.kernels_dir.join(version);
            prefer_arch_layout(base.join(&self.arch).join(KERNEL_FILE), base.join(KERNEL_FILE))
        })
    }
}

fn prefer_arch_layout(arch_path: PathBuf, legacy_path: PathBuf) -> PathBuf {
    match std::fs::metadata(&arch_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => legacy_path,
        _ => arch_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binaries(vars: &[(&str, &str)], firecracker: &str, kernel: &str) -> Binaries {
        let vars = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        Binaries::from_vars(&vars, "amd64", Path::new(firecracker), Path::new(kernel))
    }

    #[test]
    fn versions_resolve_under_the_default_directories() {
        let binaries = binaries(&[], "", "");

        assert_eq!(
            binaries.firecracker_for("v1.14.1_e2b"),
            PathBuf::from("/fc-versions/v1.14.1_e2b/firecracker")
        );
        assert_eq!(
            binaries.kernel_for("vmlinux-6.1.158"),
            PathBuf::from("/fc-kernels/vmlinux-6.1.158/vmlinux.bin")
        );
    }

    #[test]
    fn the_arch_layout_wins_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        let arch_kernel = dir.path().join("kernels/vmlinux-6.1/amd64");
        std::fs::create_dir_all(&arch_kernel).unwrap();
        std::fs::write(arch_kernel.join("vmlinux.bin"), "").unwrap();
        let binaries = binaries(
            &[
                ("HOST_KERNELS_DIR", &format!("{root}/kernels")),
                ("FIRECRACKER_VERSIONS_DIR", &format!("{root}/fc")),
            ],
            "",
            "",
        );

        assert_eq!(binaries.kernel_for("vmlinux-6.1"), arch_kernel.join("vmlinux.bin"));
        assert_eq!(
            binaries.firecracker_for("v1"),
            dir.path().join("fc/v1/firecracker"),
            "a missing arch directory falls back to the legacy flat layout"
        );
    }

    #[test]
    fn explicit_paths_override_every_version() {
        let binaries = binaries(&[], "/opt/fc", "/opt/vmlinux");

        assert_eq!(binaries.firecracker_for("any"), PathBuf::from("/opt/fc"));
        assert_eq!(binaries.kernel_for("any"), PathBuf::from("/opt/vmlinux"));
    }
}
