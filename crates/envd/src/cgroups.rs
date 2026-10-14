pub mod freeze;
pub mod hierarchy;
pub mod memory;
pub mod spawn;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::util::fs::os_error;

const MEGABYTE: u64 = 1024 * 1024;
const MAX_MEMORY_RESERVED: u64 = 128 * MEGABYTE;
#[cfg(target_os = "linux")]
const CGROUP2_SUPER_MAGIC: i64 = 0x6367_7270;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessType {
    Pty,
    User,
    Socat,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CgroupSpec {
    process_type: ProcessType,
    name: &'static str,
    properties: Vec<(&'static str, String)>,
}

struct Cgroup {
    path: PathBuf,
    procs: Arc<File>,
}

#[derive(Default)]
pub struct CgroupManager {
    root: PathBuf,
    cgroups: HashMap<ProcessType, Cgroup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Vanished,
    Unobservable,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupError {
    pub failure: Failure,
    pub message: String,
}

impl std::fmt::Display for CgroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl CgroupError {
    pub fn vanished(&self) -> bool {
        self.failure == Failure::Vanished
    }

    fn unobservable() -> Self {
        Self {
            failure: Failure::Unobservable,
            message: "cgroup freeze state unobservable".to_owned(),
        }
    }

    fn file(op: &str, path: &Path, error: &io::Error) -> Self {
        let vanished = error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(nix::libc::ENODEV);
        Self {
            failure: if vanished { Failure::Vanished } else { Failure::Other },
            message: os_error(op, path, error),
        }
    }
}

#[derive(Clone)]
pub struct Placement {
    procs: Arc<File>,
}

impl CgroupManager {
    pub fn create(config: &Config, mem_total: impl FnOnce() -> io::Result<u64>) -> Self {
        if config.no_cgroups {
            eprintln!("cgroups disabled via --no-cgroups; using no-op cgroup manager");
            return Self::default();
        }
        let created = mem_total()
            .map_err(|e| format!("failed to calculate host metrics: {e}"))
            .and_then(|mem_total| {
                Self::cgroup2(Path::new(&config.cgroup_root), specs(mem_total))
                    .map_err(|e| format!("failed to create cgroup2 manager: {e}"))
            });
        created.unwrap_or_else(|e| {
            eprintln!("{e}");
            eprintln!("falling back to no-op cgroup manager");
            Self::default()
        })
    }

    #[cfg(test)]
    pub(crate) fn at_paths(root: &Path, paths: &[(ProcessType, PathBuf)]) -> Self {
        let cgroups = paths
            .iter()
            .map(|(process_type, path)| {
                let procs = OpenOptions::new().write(true).open("/dev/null").unwrap();
                let cgroup = Cgroup {
                    path: path.clone(),
                    procs: Arc::new(procs),
                };
                (*process_type, cgroup)
            })
            .collect();
        Self {
            root: root.to_path_buf(),
            cgroups,
        }
    }

    pub fn placement(&self, process_type: ProcessType) -> Option<Placement> {
        self.cgroups.get(&process_type).map(|cgroup| Placement {
            procs: cgroup.procs.clone(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_of(&self, process_type: ProcessType) -> Option<&Path> {
        self.cgroups.get(&process_type).map(|cgroup| cgroup.path.as_path())
    }

    pub fn walk_root(&self) -> Option<&Path> {
        (!self.cgroups.is_empty()).then_some(self.root.as_path())
    }

    pub fn freeze(&self, process_type: ProcessType) -> Result<(), CgroupError> {
        self.path_of(process_type).map_or(Ok(()), freeze_at)
    }

    pub fn unfreeze(&self, process_type: ProcessType) -> Result<(), CgroupError> {
        self.path_of(process_type).map_or(Ok(()), unfreeze_at)
    }

    pub fn frozen(&self, process_type: ProcessType) -> Result<bool, CgroupError> {
        self.path_of(process_type)
            .map_or(Err(CgroupError::unobservable()), frozen_at)
    }

    fn cgroup2(root: &Path, specs: Vec<CgroupSpec>) -> Result<Self, String> {
        ensure_cgroup2(root)?;
        let (created, failed): (Vec<_>, Vec<_>) = specs
            .into_iter()
            .map(|spec| {
                create_cgroup(&root.join(spec.name), &spec.properties)
                    .map(|cgroup| (spec.process_type, cgroup))
                    .map_err(|e| format!("failed to create {} cgroup: {e}", process_type_name(spec.process_type)))
            })
            .partition(Result::is_ok);
        match failed.is_empty() {
            true => Ok(Self {
                root: root.to_path_buf(),
                cgroups: created.into_iter().filter_map(Result::ok).collect(),
            }),
            false => Err(format!(
                "failed to create cgroups: {}",
                failed
                    .into_iter()
                    .filter_map(Result::err)
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        }
    }
}

impl Placement {
    pub fn raw_fd(&self) -> RawFd {
        self.procs.as_raw_fd()
    }

    pub fn join(fd: RawFd) -> io::Result<()> {
        nix::unistd::write(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, b"0")
            .map(|_| ())
            .map_err(io::Error::from)
    }
}

fn specs(mem_total: u64) -> Vec<CgroupSpec> {
    let memory_max = mem_total - (mem_total / 8).min(MAX_MEMORY_RESERVED);
    let memory_high = memory_max;
    vec![
        CgroupSpec {
            process_type: ProcessType::Pty,
            name: "ptys",
            properties: vec![
                ("cpu.weight", "200".to_owned()),
                ("io.weight", "default 50".to_owned()),
                ("memory.high", memory_high.to_string()),
                ("memory.max", memory_max.to_string()),
            ],
        },
        CgroupSpec {
            process_type: ProcessType::Socat,
            name: "socats",
            properties: vec![
                ("cpu.weight", "150".to_owned()),
                ("io.weight", "default 50".to_owned()),
                ("memory.min", (5 * MEGABYTE).to_string()),
                ("memory.low", (8 * MEGABYTE).to_string()),
            ],
        },
        CgroupSpec {
            process_type: ProcessType::User,
            name: "user",
            properties: vec![
                ("memory.high", memory_high.to_string()),
                ("memory.max", memory_max.to_string()),
                ("cpu.weight", "50".to_owned()),
                ("io.weight", "default 10".to_owned()),
            ],
        },
    ]
}

pub fn process_type_name(process_type: ProcessType) -> &'static str {
    match process_type {
        ProcessType::Pty => "pty",
        ProcessType::User => "user",
        ProcessType::Socat => "socat",
        ProcessType::System => "system",
    }
}

#[cfg(target_os = "linux")]
fn ensure_cgroup2(root: &Path) -> Result<(), String> {
    let stats = nix::sys::statfs::statfs(root).map_err(|e| {
        format!(
            "failed to statfs cgroup root {}: {}",
            root.display(),
            io::Error::from(e)
        )
    })?;
    let kind = stats.filesystem_type().0 as i64;
    match kind == CGROUP2_SUPER_MAGIC {
        true => Ok(()),
        false => Err(format!(
            "cgroup root {} is not a cgroup2 filesystem (type=0x{kind:x})",
            root.display()
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn ensure_cgroup2(root: &Path) -> Result<(), String> {
    Err(format!("cgroup root {} is not a cgroup2 filesystem", root.display()))
}

fn create_cgroup(path: &Path, properties: &[(&str, String)]) -> Result<Cgroup, String> {
    std::fs::create_dir_all(path)
        .map_err(|e| format!("failed to create cgroup root: {}", crate::util::errno::message(&e)))?;
    let failed: Vec<String> = properties
        .iter()
        .filter_map(|(name, value)| match write_property(&path.join(name), value) {
            Ok(()) => None,
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied) => {
                eprintln!(
                    "cgroup property {name:?} unavailable at {:?}, skipping",
                    path.display().to_string()
                );
                None
            }
            Err(e) => Some(format!(
                "failed to write cgroup property {name:?}: {}",
                crate::util::errno::message(&e)
            )),
        })
        .collect();
    if !failed.is_empty() {
        return Err(failed.join("\n"));
    }
    OpenOptions::new()
        .write(true)
        .open(path.join("cgroup.procs"))
        .map(|procs| Cgroup {
            path: path.to_path_buf(),
            procs: Arc::new(procs),
        })
        .map_err(|e| crate::util::errno::message(&e))
}

fn write_property(path: &Path, value: &str) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?
        .write_all(value.as_bytes())
}

fn write_file(path: &Path, value: &str) -> Result<(), CgroupError> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| CgroupError::file("open", path, &e))?;
    file.write_all(value.as_bytes())
        .map_err(|e| CgroupError::file("write", path, &e))
}

fn read_file(path: &Path) -> Result<String, CgroupError> {
    std::fs::read_to_string(path).map_err(|e| CgroupError::file("open", path, &e))
}

pub fn freeze_at(path: &Path) -> Result<(), CgroupError> {
    write_file(&path.join("cgroup.freeze"), "1")
}

pub fn unfreeze_at(path: &Path) -> Result<(), CgroupError> {
    write_file(&path.join("cgroup.freeze"), "0")
}

pub fn frozen_at(path: &Path) -> Result<bool, CgroupError> {
    read_file(&path.join("cgroup.events")).map(|events| frozen_from_events(&events))
}

pub fn freeze_requested_at(path: &Path) -> Result<bool, CgroupError> {
    read_file(&path.join("cgroup.freeze")).map(|requested| requested.trim() == "1")
}

pub fn children_of(path: &Path) -> Result<Vec<PathBuf>, CgroupError> {
    let entries = std::fs::read_dir(path).map_err(|e| CgroupError::file("open", path, &e))?;
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| path.join(entry.file_name()))
        .collect();
    children.sort();
    Ok(children)
}

fn frozen_from_events(events: &str) -> bool {
    events
        .lines()
        .filter_map(|line| line.trim().split_once(' '))
        .find(|(key, _)| *key == "frozen")
        .is_some_and(|(_, value)| value == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_follow_upstream() {
        let gib = 1024 * MEGABYTE;
        let two_gib = specs(2 * gib);
        let ptys = &two_gib[0];
        assert_eq!((ptys.process_type, ptys.name), (ProcessType::Pty, "ptys"));
        assert_eq!(
            ptys.properties,
            vec![
                ("cpu.weight", "200".to_owned()),
                ("io.weight", "default 50".to_owned()),
                ("memory.high", (2 * gib - 128 * MEGABYTE).to_string()),
                ("memory.max", (2 * gib - 128 * MEGABYTE).to_string()),
            ]
        );
        assert_eq!(specs(512 * MEGABYTE)[2].properties[1].1, (448 * MEGABYTE).to_string());
        assert_eq!(two_gib[1].properties[2], ("memory.min", (5 * MEGABYTE).to_string()));
    }

    #[test]
    fn no_cgroups_and_bad_roots_fall_back_to_no_op() {
        let disabled = Config {
            no_cgroups: true,
            ..Config::default()
        };
        assert!(
            CgroupManager::create(&disabled, || Ok(1))
                .placement(ProcessType::User)
                .is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let plain_dir = Config {
            cgroup_root: dir.path().display().to_string(),
            ..Config::default()
        };
        let manager = CgroupManager::create(&plain_dir, || Ok(1024 * MEGABYTE));
        assert!(manager.placement(ProcessType::Pty).is_none());
        assert!(!dir.path().join("ptys").exists());
    }

    #[test]
    fn a_cgroup_dir_gets_its_properties_and_skips_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("cgroup.procs"), "").unwrap();
        std::fs::write(path.join("cpu.weight"), "100").unwrap();

        let cgroup = create_cgroup(
            &path,
            &[("cpu.weight", "50".to_owned()), ("memory.max", "1".to_owned())],
        );

        assert!(cgroup.is_ok());
        assert_eq!(std::fs::read_to_string(path.join("cpu.weight")).unwrap(), "50");
        assert!(!path.join("memory.max").exists());
    }
}
