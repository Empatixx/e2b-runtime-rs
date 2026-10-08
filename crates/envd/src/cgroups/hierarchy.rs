use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use super::freeze::FreezeMode;
use super::{children_of, freeze_requested_at, frozen_at};
use crate::util::fs::{clean, os_error};

pub const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";
const AUDIT_PATH_SAMPLE: usize = 8;
const LIVENESS_ALLOWLIST: [&str; 6] = [
    "init.scope",
    "system.slice/systemd-journald.service",
    "system.slice/rpcbind.service",
    "system.slice/rpcbind.socket",
    "system.slice/rpc-statd.service",
    "socats",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditResult {
    pub applicable: bool,
    pub visited: usize,
    pub frozen: usize,
    pub escaped: usize,
    pub truncated: bool,
    pub violation_paths: Vec<String>,
    pub escaped_paths: Vec<String>,
    pub violations: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestScan {
    pub frozen: HashSet<PathBuf>,
    pub truncated: bool,
    pub errors: Vec<String>,
}

pub fn self_cgroup_path(proc_file: &Path, root: &Path) -> Result<PathBuf, String> {
    let content = std::fs::read_to_string(proc_file)
        .map_err(|e| format!("open {}: {}", proc_file.display(), os_error("open", proc_file, &e)))?;
    content
        .lines()
        .map(|line| line.splitn(3, ':').collect::<Vec<_>>())
        .find(|parts| parts.len() == 3 && parts[0] == "0")
        .map(|parts| join(root, parts[2]))
        .ok_or_else(|| format!("no cgroup2 (0::) entry in {}", proc_file.display()))
}

pub fn ancestor_chain(root: &Path, own: &Path) -> Vec<PathBuf> {
    let root = clean(root);
    let own = clean(own);
    match own.strip_prefix(&root) {
        Ok(relative) => std::iter::once(root.clone())
            .chain(relative.components().scan(root.clone(), |current, segment| {
                current.push(segment);
                Some(current.clone())
            }))
            .collect(),
        Err(_) => vec![root],
    }
}

pub fn descend_set(root: &Path, own: &Path) -> Vec<PathBuf> {
    let allowlisted_parents = LIVENESS_ALLOWLIST.iter().flat_map(|entry| {
        let full = join(root, entry);
        ancestor_chain(root, full.parent().unwrap_or(root))
    });
    let mut seen = HashSet::new();
    ancestor_chain(root, own)
        .into_iter()
        .chain(allowlisted_parents)
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

pub fn allowlisted(root: &Path, path: &Path) -> bool {
    clean(path)
        .strip_prefix(clean(root))
        .ok()
        .map(|relative| relative.to_string_lossy().into_owned())
        .is_some_and(|relative| {
            LIVENESS_ALLOWLIST
                .iter()
                .any(|entry| relative == *entry || relative.starts_with(&format!("{entry}/")))
        })
}

pub fn sample_path(into: &mut Vec<String>, path: &Path) {
    if into.len() < AUDIT_PATH_SAMPLE {
        into.push(path.display().to_string());
    }
}

fn sample_relative(into: &mut Vec<String>, root: &Path, path: &Path) {
    if let (true, Ok(relative)) = (into.len() < AUDIT_PATH_SAMPLE, clean(path).strip_prefix(clean(root))) {
        into.push(relative.display().to_string());
    }
}

pub fn audit_frozen_state(
    root: &Path,
    proc_file: &Path,
    max_cgroups: usize,
    sweep: Option<FreezeMode>,
) -> Result<AuditResult, String> {
    let mut result = AuditResult::default();
    if sweep != Some(FreezeMode::Hierarchy) {
        return Ok(result);
    }
    let own = self_cgroup_path(proc_file, root).map_err(|e| format!("locate envd cgroup for the audit: {e}"))?;
    let exempt: HashSet<PathBuf> = descend_set(root, &own).into_iter().collect();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(current) = queue.pop_front() {
        if result.visited >= max_cgroups {
            result.truncated = true;
            break;
        }
        result.visited += 1;
        if let Ok(children) = children_of(&current) {
            queue.extend(children);
        }
        if current == root {
            continue;
        }
        let Ok(frozen) = frozen_at(&current) else {
            continue;
        };
        let protected = exempt.contains(&current) || allowlisted(root, &current);
        match (frozen, protected) {
            (true, true) => {
                result.violations += 1;
                sample_relative(&mut result.violation_paths, root, &current);
            }
            (true, false) => result.frozen += 1,
            (false, false) => {
                result.escaped += 1;
                sample_relative(&mut result.escaped_paths, root, &current);
            }
            (false, true) => {}
        }
    }
    result.applicable = true;
    Ok(result)
}

pub fn scan_guest_frozen(root: &Path, proc_file: &Path, max_cgroups: usize) -> GuestScan {
    let mut scan = GuestScan::default();
    let own = match self_cgroup_path(proc_file, root) {
        Ok(own) => own,
        Err(e) => {
            scan.errors
                .push(format!("locate envd cgroup for the guest-freeze scan: {e}"));
            return scan;
        }
    };
    let ours: HashSet<PathBuf> = descend_set(root, &own).into_iter().collect();
    let mut visited = 0;
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(current) = queue.pop_front() {
        if visited >= max_cgroups {
            scan.truncated = true;
            return scan;
        }
        visited += 1;
        if !ours.contains(&current) && !allowlisted(root, &current) {
            match freeze_requested_at(&current) {
                Ok(true) => {
                    scan.frozen.insert(current.clone());
                }
                Ok(false) => {}
                Err(e) if e.vanished() => {}
                Err(e) => scan
                    .errors
                    .push(format!("read freeze state of {}: {e}", current.display())),
            }
        }
        match children_of(&current) {
            Ok(children) => queue.extend(children),
            Err(e) if e.vanished() => {}
            Err(e) => scan.errors.push(format!("list children of {}: {e}", current.display())),
        }
    }
    scan
}

pub fn join(root: &Path, relative: &str) -> PathBuf {
    clean(&root.join(relative.trim_start_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    #[test]
    fn finds_the_own_cgroup_and_its_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let proc_file = dir.path().join("cgroup");
        std::fs::write(&proc_file, "0::/system.slice/envd.service\n").unwrap();

        let own = self_cgroup_path(&proc_file, Path::new("/sys/fs/cgroup")).unwrap();

        assert_eq!(own, path("/sys/fs/cgroup/system.slice/envd.service"));
        assert_eq!(
            ancestor_chain(Path::new("/sys/fs/cgroup"), &own),
            vec![
                path("/sys/fs/cgroup"),
                path("/sys/fs/cgroup/system.slice"),
                path("/sys/fs/cgroup/system.slice/envd.service")
            ]
        );
        assert_eq!(ancestor_chain(Path::new("/a"), Path::new("/b")), vec![path("/a")]);
        std::fs::write(&proc_file, "1:cpu:/x\n").unwrap();
        assert_eq!(
            self_cgroup_path(&proc_file, Path::new("/r")),
            Err(format!("no cgroup2 (0::) entry in {}", proc_file.display()))
        );
    }

    #[test]
    fn the_descend_set_covers_envd_and_the_allowlisted_parents() {
        let set = descend_set(Path::new("/r"), Path::new("/r/system.slice/envd.service"));
        assert_eq!(
            set,
            vec![
                path("/r"),
                path("/r/system.slice"),
                path("/r/system.slice/envd.service")
            ]
        );
        assert!(allowlisted(
            Path::new("/r"),
            Path::new("/r/system.slice/rpcbind.service")
        ));
        assert!(allowlisted(Path::new("/r"), Path::new("/r/socats/child")));
        assert!(!allowlisted(Path::new("/r"), Path::new("/r/socatsx")));
    }
}
