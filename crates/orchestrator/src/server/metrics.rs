use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use sysinfo::{Disks, System};

use crate::proto::{DiskMetrics, MachineInfo};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
const SKIPPED_MOUNT_PREFIX: &str = "/boot/";
const SKIPPED_DEVICE_PREFIXES: [&str; 2] = ["/dev/loop", "/dev/zram"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostSample {
    pub cpu_percent: u32,
    pub cpu_count: u32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub disks: Vec<DiskMetrics>,
    pub hugepages: Hugepages,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hugepages {
    pub total: u64,
    pub used: u64,
    pub reserved: u64,
    pub size_bytes: u64,
}

#[derive(Default)]
pub struct HostMetrics {
    latest: RwLock<HostSample>,
}

impl HostMetrics {
    pub fn latest(&self) -> HostSample {
        self.latest.read().clone()
    }

    pub async fn run(self: Arc<Self>) {
        let mut system = System::new();
        let mut ticks = tokio::time::interval(SAMPLE_INTERVAL);
        loop {
            ticks.tick().await;
            *self.latest.write() = sample(&mut system);
        }
    }
}

fn sample(system: &mut System) -> HostSample {
    system.refresh_cpu_usage();
    system.refresh_memory();
    HostSample {
        cpu_percent: system.global_cpu_usage() as u32,
        cpu_count: std::thread::available_parallelism().map_or(0, |count| count.get() as u32),
        memory_used_bytes: system.used_memory(),
        memory_total_bytes: system.total_memory(),
        disks: disks(),
        hugepages: std::fs::read_to_string("/proc/meminfo")
            .map(|meminfo| hugepages(&meminfo))
            .unwrap_or_default(),
    }
}

fn disks() -> Vec<DiskMetrics> {
    Disks::new_with_refreshed_list()
        .list()
        .iter()
        .map(|disk| DiskMetrics {
            mount_point: disk.mount_point().to_string_lossy().into_owned(),
            device: disk.name().to_string_lossy().into_owned(),
            filesystem_type: disk.file_system().to_string_lossy().into_owned(),
            used_bytes: disk.total_space().saturating_sub(disk.available_space()),
            total_bytes: disk.total_space(),
        })
        .filter(|disk| {
            !disk.mount_point.starts_with(SKIPPED_MOUNT_PREFIX)
                && !SKIPPED_DEVICE_PREFIXES
                    .iter()
                    .any(|prefix| disk.device.starts_with(prefix))
        })
        .collect()
}

pub fn hugepages(meminfo: &str) -> Hugepages {
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let total = field("HugePages_Total");
    Hugepages {
        total,
        used: total.saturating_sub(field("HugePages_Free")),
        reserved: field("HugePages_Rsvd"),
        size_bytes: field("Hugepagesize") * 1024,
    }
}

pub fn machine_info(cpuinfo: &str, arch: &str) -> MachineInfo {
    let field = |name: &str| {
        cpuinfo
            .lines()
            .take_while(|line| !line.trim().is_empty())
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim() == name)
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or_default()
    };
    let go_arch = go_arch(arch);
    let family = field("cpu family");
    let model = field("model");
    let arm = go_arch == "arm64";
    MachineInfo {
        cpu_architecture: go_arch.to_owned(),
        cpu_family: if family.is_empty() && arm {
            go_arch.to_owned()
        } else {
            family
        },
        cpu_model: if model.is_empty() && arm { "0".to_owned() } else { model },
        cpu_model_name: field("model name"),
        cpu_flags: field("flags").split_whitespace().map(str::to_owned).collect(),
    }
}

fn go_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hugepages_come_from_meminfo() {
        let meminfo = "MemTotal:       16000000 kB\nHugePages_Total:     512\nHugePages_Free:      200\nHugePages_Rsvd:       10\nHugepagesize:       2048 kB\n";

        assert_eq!(
            hugepages(meminfo),
            Hugepages {
                total: 512,
                used: 312,
                reserved: 10,
                size_bytes: 2 * 1024 * 1024
            }
        );
        assert_eq!(hugepages(""), Hugepages::default());
    }

    #[test]
    fn machine_info_uses_go_arch_names_and_the_first_cpu() {
        let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 85\nmodel name\t: Intel(R) Xeon(R)\nflags\t\t: fpu vme sse\n\nprocessor\t: 1\nmodel\t\t: 99\n";

        assert_eq!(
            machine_info(cpuinfo, "x86_64"),
            MachineInfo {
                cpu_architecture: "amd64".to_owned(),
                cpu_family: "6".to_owned(),
                cpu_model: "85".to_owned(),
                cpu_model_name: "Intel(R) Xeon(R)".to_owned(),
                cpu_flags: vec!["fpu".to_owned(), "vme".to_owned(), "sse".to_owned()],
            }
        );
    }

    #[test]
    fn arm_without_family_or_model_falls_back_like_upstream() {
        let info = machine_info("processor\t: 0\nFeatures\t: fp asimd\n", "aarch64");

        assert_eq!(
            (
                info.cpu_architecture.as_str(),
                info.cpu_family.as_str(),
                info.cpu_model.as_str()
            ),
            ("arm64", "arm64", "0")
        );
    }
}
