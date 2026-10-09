pub mod hash;
pub mod layers;
pub mod render;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::builder::hash::{ENVD_MEMORY_LOW_MIB, ENVD_MEMORY_MIN_MIB};
use crate::go_os::path_error;

use render::{Value, Values, render};

pub const PROVISION_LOG_PREFIX: &str = "[external] ";
pub const PROVISION_RESULT_PATH: &str = "/provision.result";
pub const PROVISION_EXIT_PREFIX: &str = "E2B_PROVISIONING_EXIT:";
pub const BUSYBOX_PATH: &str = "usr/bin/busybox";
pub const BUSYBOX_INIT_PATH: &str = "usr/bin/init";
pub const GUEST_ENVD_PATH: &str = "/usr/bin/envd";
const HOSTNAME: &str = "e2b.local";
const NAMESERVER: &str = "8.8.8.8";
const MEMORY_LIMIT_CAP_MB: i64 = 512;
const PRESET_PATH: &str = "etc/systemd/system-preset/00-e2b.preset";
const PRESET: &str = "enable envd.service\n";
const PROVISION_SCRIPT_PATH: &str = "usr/local/bin/provision.sh";

pub const PROVISION_SCRIPT: &str = include_str!("../../assets/template/provision.sh");
pub const DISTRO_SELECTOR: &str = include_str!("../../assets/template/distro-selector.sh");

pub const TEMPLATES: [(&str, &str); 16] = [
    (
        "chrony-source.openrc.tpl",
        include_str!("../../assets/template/files/chrony-source.openrc.tpl"),
    ),
    (
        "chrony-source.service.tpl",
        include_str!("../../assets/template/files/chrony-source.service.tpl"),
    ),
    (
        "chrony-source.sh.tpl",
        include_str!("../../assets/template/files/chrony-source.sh.tpl"),
    ),
    (
        "disable-watchdog.service.tpl",
        include_str!("../../assets/template/files/disable-watchdog.service.tpl"),
    ),
    (
        "envd.openrc.tpl",
        include_str!("../../assets/template/files/envd.openrc.tpl"),
    ),
    (
        "envd.service.tpl",
        include_str!("../../assets/template/files/envd.service.tpl"),
    ),
    ("hostname.tpl", include_str!("../../assets/template/files/hostname.tpl")),
    ("hosts.tpl", include_str!("../../assets/template/files/hosts.tpl")),
    ("inittab.tpl", include_str!("../../assets/template/files/inittab.tpl")),
    (
        "journald.conf.tpl",
        include_str!("../../assets/template/files/journald.conf.tpl"),
    ),
    (
        "provision-runner.sh.tpl",
        include_str!("../../assets/template/files/provision-runner.sh.tpl"),
    ),
    ("rcS.sh.tpl", include_str!("../../assets/template/files/rcS.sh.tpl")),
    (
        "resolv.conf.tpl",
        include_str!("../../assets/template/files/resolv.conf.tpl"),
    ),
    (
        "seed-certs.sh.tpl",
        include_str!("../../assets/template/files/seed-certs.sh.tpl"),
    ),
    (
        "serial-getty-autologin.service.tpl",
        include_str!("../../assets/template/files/serial-getty-autologin.service.tpl"),
    ),
    (
        "system.slice.d-envd.conf.tpl",
        include_str!("../../assets/template/files/system.slice.d-envd.conf.tpl"),
    ),
];

pub const SYMLINKS: [(&str, &str); 1] = [(
    "etc/systemd/system/multi-user.target.wants/envd.service",
    "/etc/systemd/system/envd.service",
)];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerFile {
    pub bytes: Vec<u8>,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedFiles {
    pub envd: Vec<u8>,
    pub busybox: Vec<u8>,
    pub provision_script: String,
    pub memory_mb: i64,
    pub envd_memory_protection: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedSources {
    pub envd: PathBuf,
    pub busybox: PathBuf,
    pub provision_script: String,
    pub memory_mb: i64,
    pub envd_memory_protection: bool,
}

impl InjectedSources {
    pub fn read(&self) -> Result<InjectedFiles, String> {
        let envd = std::fs::read(&self.envd)
            .map_err(|e| format!("error reading envd file: {}", path_error("open", &self.envd, &e)))?;
        let busybox = std::fs::read(&self.busybox).map_err(|e| {
            format!(
                "error reading busybox file {}: {}",
                self.busybox.display(),
                path_error("open", &self.busybox, &e)
            )
        })?;
        Ok(InjectedFiles {
            envd,
            busybox,
            provision_script: self.provision_script.clone(),
            memory_mb: self.memory_mb,
            envd_memory_protection: self.envd_memory_protection,
        })
    }
}

impl InjectedFiles {
    pub fn files(&self) -> Result<BTreeMap<String, LayerFile>, String> {
        let fixed = [
            (GUEST_ENVD_PATH, self.envd.clone(), 0o777),
            (PRESET_PATH, PRESET.as_bytes().to_vec(), 0o644),
            (PROVISION_SCRIPT_PATH, self.provision_script.as_bytes().to_vec(), 0o777),
            (BUSYBOX_PATH, self.busybox.clone(), 0o755),
            (BUSYBOX_INIT_PATH, self.busybox.clone(), 0o755),
        ]
        .map(|(path, bytes, mode)| (path.to_owned(), LayerFile { bytes, mode }));
        TEMPLATES
            .iter()
            .map(|(name, source)| {
                self.generate(name, source)
                    .map_err(|e| format!("error generating file from {name:?}: {e}"))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(|generated| fixed.into_iter().chain(generated.into_iter().flatten()).collect())
    }

    fn generate(&self, name: &str, source: &str) -> Result<Vec<(String, LayerFile)>, String> {
        let rendered = render(name, source, &self.values())?;
        let data = rendered.text.trim().as_bytes().to_vec();
        if rendered.files.is_empty() && !data.is_empty() {
            return Err(format!("template {name:?} did not set path"));
        }
        Ok(rendered
            .files
            .into_iter()
            .map(|file| {
                (
                    file.path.strip_prefix('/').unwrap_or(&file.path).to_owned(),
                    LayerFile {
                        bytes: data.clone(),
                        mode: file.mode,
                    },
                )
            })
            .collect())
    }

    fn values(&self) -> Values {
        Values::from([
            ("Hostname", Value::Text(HOSTNAME.to_owned())),
            ("ProvisionLogPrefix", Value::Text(PROVISION_LOG_PREFIX.to_owned())),
            ("ProvisionResultPath", Value::Text(PROVISION_RESULT_PATH.to_owned())),
            ("ProvisionExitPrefix", Value::Text(PROVISION_EXIT_PREFIX.to_owned())),
            ("Nameserver", Value::Text(NAMESERVER.to_owned())),
            (
                "MemoryLimit",
                Value::Number((self.memory_mb / 2).min(MEMORY_LIMIT_CAP_MB)),
            ),
            ("EnvdMemoryProtection", Value::Flag(self.envd_memory_protection)),
            ("EnvdMemoryMinMiB", Value::Number(ENVD_MEMORY_MIN_MIB)),
            ("EnvdMemoryLowMiB", Value::Number(ENVD_MEMORY_LOW_MIB)),
        ])
    }
}

pub fn provision_script(provider: &str) -> Result<String, String> {
    let values = Values::from([
        ("BusyBox", Value::Text(format!("/{BUSYBOX_PATH}"))),
        ("ResultPath", Value::Text(PROVISION_RESULT_PATH.to_owned())),
        ("Provider", Value::Text(provider.to_owned())),
        ("DistroSelector", Value::Text(DISTRO_SELECTOR.to_owned())),
    ]);
    render("provisioning-script", PROVISION_SCRIPT, &values)
        .map(|rendered| rendered.text)
        .map_err(|e| format!("error executing provision script: {e}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/template");

    fn injected(memory_mb: i64, envd_memory_protection: bool) -> InjectedFiles {
        InjectedFiles {
            envd: b"echo hello".to_vec(),
            busybox: b"busybox-binary".to_vec(),
            provision_script: "provision.sh".to_owned(),
            memory_mb,
            envd_memory_protection,
        }
    }

    fn text(files: &BTreeMap<String, LayerFile>, path: &str) -> String {
        String::from_utf8(files[path].bytes.clone()).unwrap()
    }

    #[test]
    fn rendered_files_match_the_upstream_output() {
        let files = injected(1024, false).files().unwrap();
        let golden = Path::new(GOLDEN).join("rendered");

        let checked = files
            .iter()
            .filter(|(path, _)| golden.join(path).is_file())
            .map(|(path, file)| assert_eq!(file.bytes, std::fs::read(golden.join(path)).unwrap(), "{path}"))
            .count();

        assert_eq!(checked, 16);
    }

    #[test]
    fn the_files_layer_has_the_upstream_entries() {
        let files = injected(100, false).files().unwrap();

        assert_eq!(files.len(), 21);
        assert_eq!(text(&files, PRESET_PATH), "enable envd.service\n");
        assert_eq!(text(&files, "etc/hostname"), "e2b.local");
        assert_eq!(text(&files, "etc/resolv.conf"), "nameserver 8.8.8.8");
        assert!(text(&files, "etc/systemd/system/envd.service").contains("\"GOMEMLIMIT=50MiB\""));
        assert!(text(&files, "usr/local/share/e2b/envd.openrc").contains("GOMEMLIMIT=50MiB"));
        assert!(text(&files, "usr/local/bin/e2b-provision-runner").contains("#!/usr/bin/busybox ash"));
        assert_eq!(
            text(&files, "etc/systemd/system/systemd-journald.service.d/override.conf"),
            "[Service]\nWatchdogSec=0"
        );
        assert_eq!(files[GUEST_ENVD_PATH].mode, 0o777);
        assert_eq!(files["etc/inittab"].mode, 0o777);
    }

    #[test]
    fn envd_memory_protection_adds_the_slice_drop_in() {
        let files = injected(4096, true).files().unwrap();

        assert_eq!(files.len(), 22);
        let slice = text(&files, "etc/systemd/system/system.slice.d/10-e2b-envd.conf");
        assert!(slice.contains("[Slice]") && slice.contains("MemoryMin=128M\nMemoryLow=256M"));
        assert!(text(&files, "etc/systemd/system/envd.service").contains("MemoryMin=128M\nMemoryLow=256M"));
    }

    #[test]
    fn the_provision_script_matches_the_upstream_render() {
        let golden = std::fs::read_to_string(Path::new(GOLDEN).join("provision.rendered.sh")).unwrap();

        assert_eq!(provision_script("gcp").unwrap(), golden);
    }
}
