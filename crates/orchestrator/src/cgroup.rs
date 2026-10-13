pub mod cpu_limit;
pub mod removal;

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::hoststats::{CgroupSample, parse_cpu_stat};

const ROOT: &str = "/sys/fs/cgroup/e2b";
const PROC_ROOT: &str = "/proc/";
const CONTROLLERS: &str = "+cpu +memory";

pub struct Cgroup {
    path: PathBuf,
    peak: Option<File>,
    proc_root: PathBuf,
    vcpu_threads_confined: bool,
    removed: bool,
}

pub struct CgroupStats {
    path: PathBuf,
    peak: Option<File>,
}

impl Cgroup {
    pub fn create(name: &str) -> io::Result<Self> {
        std::fs::create_dir_all(ROOT)?;
        if let Err(e) = std::fs::write(Path::new(ROOT).join("cgroup.subtree_control"), CONTROLLERS) {
            tracing::warn!(error = %e, "could not enable cpu and memory controllers for sandboxes");
        }
        let path = Path::new(ROOT).join(name);
        std::fs::create_dir_all(&path)?;
        let peak = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("memory.peak"))
            .inspect_err(|e| tracing::warn!(error = %e, "failed to open memory.peak for reset"))
            .ok();
        Ok(Self {
            path,
            peak,
            proc_root: PathBuf::from(PROC_ROOT),
            vcpu_threads_confined: false,
            removed: false,
        })
    }

    pub fn existing(path: PathBuf) -> Self {
        Self {
            path,
            peak: None,
            proc_root: PathBuf::from(PROC_ROOT),
            vcpu_threads_confined: false,
            removed: false,
        }
    }

    pub fn stats(&self) -> CgroupStats {
        CgroupStats {
            path: self.path.clone(),
            peak: self.peak.as_ref().and_then(|peak| peak.try_clone().ok()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn add(&self, pid: u32) -> io::Result<()> {
        std::fs::write(self.path.join("cgroup.procs"), pid.to_string())
    }

    pub fn pids(&self) -> io::Result<Vec<u32>> {
        let procs = std::fs::read_to_string(self.path.join("cgroup.procs"))?;
        Ok(procs.lines().filter_map(|line| line.trim().parse().ok()).collect())
    }
}

impl CgroupStats {
    pub fn sample(&mut self) -> io::Result<CgroupSample> {
        let (cpu_usage_usec, cpu_user_usec, cpu_system_usec) =
            parse_cpu_stat(&std::fs::read_to_string(self.path.join("cpu.stat"))?);
        let memory_usage_bytes = read_number(&std::fs::read_to_string(self.path.join("memory.current"))?);
        Ok(CgroupSample {
            cpu_usage_usec,
            cpu_user_usec,
            cpu_system_usec,
            memory_usage_bytes,
            memory_peak_bytes: self.read_and_reset_peak(),
        })
    }

    fn read_and_reset_peak(&mut self) -> u64 {
        let Some(peak) = self.peak.as_mut() else {
            return 0;
        };
        let mut text = String::new();
        let value = peak
            .seek(SeekFrom::Start(0))
            .and_then(|_| peak.read_to_string(&mut text))
            .map(|_| read_number(&text))
            .unwrap_or(0);
        if let Err(e) = peak.write_all(b"0") {
            tracing::warn!(error = %e, "failed to reset memory.peak, interval peak semantics degraded");
        }
        value
    }
}

fn read_number(text: &str) -> u64 {
    text.trim().parse().unwrap_or(0)
}
