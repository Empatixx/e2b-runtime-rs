use std::io;
use std::path::{Path, PathBuf};

const ROOT: &str = "/sys/fs/cgroup/e2b";
const CONTROLLERS: &str = "+cpu +memory";

pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    pub fn create(name: &str) -> io::Result<Self> {
        std::fs::create_dir_all(ROOT)?;
        std::fs::write(Path::new(ROOT).join("cgroup.subtree_control"), CONTROLLERS)?;
        let path = Path::new(ROOT).join(name);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
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

    pub fn remove(self) -> io::Result<()> {
        std::fs::remove_dir(&self.path)
    }
}
