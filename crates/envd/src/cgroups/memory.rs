use std::path::Path;

use serde::Serialize;

use super::hierarchy::{ancestor_chain, self_cgroup_path};
use crate::util::fs::clean;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MemoryProtection {
    pub request: u64,
    pub low: u64,
    pub floor: u64,
    pub partial: bool,
}

impl MemoryProtection {
    pub fn read(proc_file: &Path, root: Option<&Path>) -> Self {
        root.map_or(Self::unknown(), |root| Self::read_under(proc_file, &clean(root)))
    }

    fn unknown() -> Self {
        Self {
            partial: true,
            ..Self::default()
        }
    }

    fn read_under(proc_file: &Path, root: &Path) -> Self {
        let Ok(own) = self_cgroup_path(proc_file, root).map(|own| clean(&own)) else {
            return Self::unknown();
        };
        match own.strip_prefix(root) {
            Err(_) => Self::unknown(),
            Ok(relative) if relative.as_os_str().is_empty() => Self::default(),
            Ok(_) => Self::along_chain(root, &own),
        }
    }

    fn along_chain(root: &Path, own: &Path) -> Self {
        let mut protection = Self::default();
        for (index, level) in ancestor_chain(root, own).iter().skip(1).enumerate() {
            let (value, read) = memory_value(&level.join("memory.min"));
            protection.partial |= !read;
            if index == 0 || value < protection.floor {
                protection.floor = value;
            }
            if level == own {
                protection.request = value;
            }
        }
        let (low, read) = memory_value(&own.join("memory.low"));
        protection.partial |= !read;
        protection.low = low;
        protection
    }
}

fn memory_value(path: &Path) -> (u64, bool) {
    std::fs::read_to_string(path)
        .ok()
        .map(|content| content.trim().to_owned())
        .and_then(|value| match value.as_str() {
            "max" => Some(i64::MAX as u64),
            number => number.parse().ok(),
        })
        .map_or((0, false), |value| (value, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protection_is_the_lowest_memory_min_along_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let service = root.join("system.slice/envd.service");
        std::fs::create_dir_all(&service).unwrap();
        std::fs::write(root.join("system.slice/memory.min"), "max\n").unwrap();
        std::fs::write(service.join("memory.min"), "1048576\n").unwrap();
        std::fs::write(service.join("memory.low"), "2097152\n").unwrap();
        let proc_file = root.join("cgroup");
        std::fs::write(&proc_file, "0::/system.slice/envd.service\n").unwrap();

        assert_eq!(
            MemoryProtection::read(&proc_file, Some(root)),
            MemoryProtection {
                request: 1_048_576,
                low: 2_097_152,
                floor: 1_048_576,
                partial: false,
            }
        );
        std::fs::remove_file(service.join("memory.low")).unwrap();
        assert!(MemoryProtection::read(&proc_file, Some(root)).partial);
        assert_eq!(MemoryProtection::read(&proc_file, None), MemoryProtection::unknown());
        assert_eq!(
            serde_json::to_string(&MemoryProtection::unknown()).unwrap(),
            r#"{"request":0,"low":0,"floor":0,"partial":true}"#
        );
    }
}
