use std::io;

use parking_lot::Mutex;
use serde::Serialize;

use super::oom::OomKill;
use crate::util::go_json;

const KIB: u64 = 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub ts: i64,
    pub cpu_count: u32,
    #[serde(serialize_with = "go_json::float32")]
    pub cpu_used_pct: f32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_cache: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom_kills: Option<Vec<OomKill>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct CpuTimes {
    total: f64,
    busy: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Memory {
    total: u64,
    used: u64,
    cached: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Disk {
    total: u64,
    available: u64,
}

pub struct CpuUsage {
    last: Mutex<Option<CpuTimes>>,
}

impl CpuUsage {
    pub fn start() -> Self {
        Self {
            last: Mutex::new(read_proc("stat").ok().and_then(|stat| cpu_times(&stat))),
        }
    }

    fn percent(&self, now: CpuTimes) -> io::Result<f64> {
        self.last
            .lock()
            .replace(now)
            .map(|last| busy_percent(last, now))
            .ok_or_else(|| io::Error::other("error getting times for cpu percent. lastTimes was nil"))
    }
}

pub fn read_metrics(cpu: &CpuUsage) -> io::Result<Metrics> {
    let memory = memory(&read_proc("meminfo")?)?;
    let cpu_count = cpu_count(&read_proc("cpuinfo").unwrap_or_default(), &read_proc("stat")?);
    let times = cpu_times(&read_proc("stat")?).ok_or_else(|| io::Error::other("stat does not contain cpu info"))?;
    let percent = cpu.percent(times)?;
    let disk = disk("/")?;
    Ok(Metrics {
        ts: chrono::Utc::now().timestamp(),
        cpu_count,
        cpu_used_pct: rounded_percent(percent),
        mem_total: memory.total,
        mem_used: memory.used,
        mem_cache: memory.cached,
        disk_used: disk.total - disk.available,
        disk_total: disk.total,
        oom_kills: None,
    })
}

fn read_proc(name: &str) -> io::Result<String> {
    std::fs::read_to_string(format!("/proc/{name}"))
}

fn rounded_percent(percent: f64) -> f32 {
    match percent > 0.0 {
        true => ((percent * 100.0).round() / 100.0) as f32,
        false => percent as f32,
    }
}

fn memory(meminfo: &str) -> io::Result<Memory> {
    let field = |key: &str| -> io::Result<Option<u64>> {
        meminfo
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim() == key)
            .map(|(_, value)| {
                value
                    .trim()
                    .replace(" kB", "")
                    .parse::<u64>()
                    .map(|kib| kib * KIB)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .transpose()
    };
    let total = field("MemTotal")?.unwrap_or_default();
    let free = field("MemFree")?.unwrap_or_default();
    let reclaimable = field("SReclaimable")?;
    let cached = field("Cached")?.unwrap_or_default() + reclaimable.unwrap_or_default();
    let available = match field("MemAvailable")? {
        Some(available) => available,
        None => cached + free,
    };
    Ok(Memory {
        total,
        used: total.wrapping_sub(available),
        cached,
    })
}

fn cpu_times(stat: &str) -> Option<CpuTimes> {
    let fields: Vec<&str> = stat.lines().next()?.split_whitespace().collect();
    (fields.len() >= 8 && fields[0].starts_with("cpu")).then_some(())?;
    let values: Vec<f64> = fields[1..]
        .iter()
        .map(|value| value.parse().ok())
        .collect::<Option<_>>()?;
    let value = |index: usize| values.get(index).copied().unwrap_or_default();
    let (idle, iowait) = (value(3), value(4));
    let total: f64 = (0..8).map(value).sum();
    Some(CpuTimes {
        total,
        busy: total - idle - iowait,
    })
}

fn busy_percent(before: CpuTimes, after: CpuTimes) -> f64 {
    match (after.busy <= before.busy, after.total <= before.total) {
        (true, _) => 0.0,
        (false, true) => 100.0,
        (false, false) => ((after.busy - before.busy) / (after.total - before.total) * 100.0).clamp(0.0, 100.0),
    }
}

fn cpu_count(cpuinfo: &str, stat: &str) -> u32 {
    let processors = cpuinfo
        .lines()
        .map(str::to_lowercase)
        .filter(|line| line.starts_with("processor"))
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(_, number)| number.trim().parse::<i32>().is_ok())
        })
        .count();
    let counted = match processors {
        0 => stat
            .lines()
            .filter(|line| line.len() >= 4 && line.starts_with("cpu") && line.as_bytes()[3].is_ascii_digit())
            .count(),
        processors => processors,
    };
    counted as u32
}

fn disk(path: &str) -> io::Result<Disk> {
    let stats = nix::sys::statfs::statfs(path).map_err(io::Error::from)?;
    let block = stats.block_size() as u64;
    Ok(Disk {
        total: stats.blocks() as u64 * block,
        available: stats.blocks_available() as u64 * block,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:        2000 kB\nMemFree:          500 kB\nMemAvailable:    1200 kB\nBuffers:           10 kB\nCached:           300 kB\nSReclaimable:      40 kB\n";

    #[test]
    fn memory_follows_gopsutil() {
        assert_eq!(
            memory(MEMINFO).unwrap(),
            Memory {
                total: 2000 * KIB,
                used: 800 * KIB,
                cached: 340 * KIB,
            }
        );
        let without_available = MEMINFO.replace("MemAvailable:    1200 kB\n", "");
        assert_eq!(memory(&without_available).unwrap().used, (2000 - 340 - 500) * KIB);
    }

    #[test]
    fn cpu_percent_is_the_busy_share_since_the_last_call_without_guest_time() {
        let before = cpu_times("cpu  100 0 100 700 100 0 0 0 50 50\ncpu0 1 2 3 4 5 6 7\n").unwrap();
        let after = cpu_times("cpu  200 0 200 800 100 0 0 0 90 90").unwrap();
        let usage = CpuUsage {
            last: Mutex::new(Some(before)),
        };

        assert_eq!(usage.percent(after).unwrap(), 200.0 / 300.0 * 100.0);
        assert_eq!(usage.percent(after).unwrap(), 0.0);
        assert_eq!(rounded_percent(200.0 / 300.0 * 100.0), 66.67);
        assert!(cpu_times("cpu 1 2 3").is_none());
        assert!(CpuUsage { last: Mutex::new(None) }.percent(after).is_err());
    }

    #[test]
    fn cpu_count_reads_processors_then_falls_back_to_stat() {
        assert_eq!(cpu_count("processor\t: 0\nprocessor\t: 1\nmodel name\t: x\n", ""), 2);
        assert_eq!(cpu_count("", "cpu  1 2\ncpu0 1\ncpu1 1\nintr 5\n"), 2);
    }

    #[test]
    fn metrics_json_matches_upstream_shape() {
        let metrics = Metrics {
            ts: 1,
            cpu_count: 2,
            cpu_used_pct: 0.0,
            mem_total: 3,
            mem_used: 4,
            mem_cache: 5,
            disk_used: 6,
            disk_total: 7,
            oom_kills: None,
        };
        assert_eq!(
            go_json::encode(&metrics).unwrap(),
            "{\"ts\":1,\"cpu_count\":2,\"cpu_used_pct\":0,\"mem_total\":3,\"mem_used\":4,\"mem_cache\":5,\"disk_used\":6,\"disk_total\":7}\n"
        );
        let with_kills = Metrics {
            oom_kills: Some(Vec::new()),
            ..metrics
        };
        assert!(go_json::encode(&with_kills).unwrap().ends_with(",\"oom_kills\":[]}\n"));
    }
}
