pub mod firecracker;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Counter;
use tokio_util::sync::CancellationToken;

use crate::cgroup::Cgroup;
use crate::exposure::nbd::device::{connected_devices, disconnect_device};
use crate::go_os::path_error;
use crate::network::slot::Slot;

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/startupreclaim";
const FIRECRACKER: &str = "firecracker";
const NBD: &str = "nbd";
const NETWORK: &str = "network";
const CGROUP: &str = "cgroup";
const FILE: &str = "file";
const SANDBOX_CGROUP_PREFIX: &str = "sbx-";

static RECLAIMED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .u64_counter("orchestrator.startup_reclaim.reclaimed")
        .with_description("Startup reclaim resources successfully reclaimed.")
        .with_unit("{resource}")
        .build()
});

static FAILED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .u64_counter("orchestrator.startup_reclaim.failed")
        .with_description("Startup reclaim resource cleanup failures.")
        .with_unit("{resource}")
        .build()
});

#[derive(Debug, Clone, bon::Builder)]
pub struct ReclaimPaths {
    #[builder(default = PathBuf::from("/proc"))]
    pub proc_dir: PathBuf,
    #[builder(default = PathBuf::from("/var/run/netns"))]
    pub netns_dir: PathBuf,
    #[builder(default = PathBuf::from("/sys/fs/cgroup/e2b"))]
    pub cgroup_root: PathBuf,
    pub sandbox_cache_dir: PathBuf,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub reclaimed: BTreeMap<&'static str, u64>,
    pub failed: BTreeMap<&'static str, u64>,
}

impl Summary {
    pub fn has_failures(&self) -> bool {
        self.failed.values().sum::<u64>() > 0
    }

    fn record(&mut self, resource: &'static str, reclaimed: u64, failures: Vec<String>) {
        let attributes = [KeyValue::new("resource_type", resource)];
        if reclaimed > 0 {
            *self.reclaimed.entry(resource).or_default() += reclaimed;
            RECLAIMED.add(reclaimed, &attributes);
        }
        failures.into_iter().for_each(|error| {
            *self.failed.entry(resource).or_default() += 1;
            FAILED.add(1, &attributes);
            tracing::warn!(resource_type = resource, %error, "startup resource reclaim failed");
        });
    }

    fn log(&self) {
        let total_reclaimed: u64 = self.reclaimed.values().sum();
        let total_failed: u64 = self.failed.values().sum();
        let reclaimed = format!("{:?}", self.reclaimed);
        let failed = format!("{:?}", self.failed);
        match total_reclaimed > 0 || total_failed > 0 {
            true => tracing::warn!(
                reclaimed,
                failed,
                total_reclaimed,
                total_failed,
                "startup resource reclaim completed with leftover resources"
            ),
            false => tracing::info!(
                reclaimed,
                failed,
                total_reclaimed,
                total_failed,
                "startup resource reclaim completed cleanly"
            ),
        }
    }
}

pub async fn run(paths: &ReclaimPaths) -> Summary {
    let mut summary = Summary::default();
    let (reclaimed, failures) = firecracker::reclaim(&paths.proc_dir);
    summary.record(FIRECRACKER, reclaimed, failures);
    let (reclaimed, failures) = reclaim_nbd().await;
    summary.record(NBD, reclaimed, failures);
    let (reclaimed, failures) = reclaim_slots(&paths.netns_dir).await;
    summary.record(NETWORK, reclaimed, failures);
    let (reclaimed, failures) = reclaim_cgroups(&paths.cgroup_root).await;
    summary.record(CGROUP, reclaimed, failures);
    let (reclaimed, failures) = reclaim_files(&paths.sandbox_cache_dir);
    summary.record(FILE, reclaimed, failures);
    summary.log();
    summary
}

fn tally(outcomes: Vec<Result<(), String>>) -> (u64, Vec<String>) {
    let failures: Vec<String> = outcomes.iter().filter_map(|outcome| outcome.clone().err()).collect();
    ((outcomes.len() - failures.len()) as u64, failures)
}

async fn reclaim_nbd() -> (u64, Vec<String>) {
    let mut outcomes = Vec::new();
    for index in connected_devices() {
        outcomes.push(
            disconnect_device(index)
                .await
                .map_err(|e| format!("failed to disconnect nbd{index}: {e}")),
        );
    }
    tally(outcomes)
}

fn names_in(dir: &Path, keep: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut names: Vec<String> = entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| keep(name))
                .collect();
            names.sort_unstable();
            Ok(names)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(path_error("open", dir, &e)),
    }
}

pub fn slot_indices(netns_dir: &Path) -> Result<Vec<u32>, String> {
    let mut indices: Vec<u32> = names_in(netns_dir, |name| name.starts_with("ns-"))
        .map_err(|e| format!("error reading netns directory: {e}"))?
        .iter()
        .filter(|name| !netns_dir.join(name).is_dir())
        .filter_map(|name| name.strip_prefix("ns-")?.parse().ok())
        .collect();
    indices.sort_unstable();
    Ok(indices)
}

async fn reclaim_slots(netns_dir: &Path) -> (u64, Vec<String>) {
    let indices = match slot_indices(netns_dir) {
        Ok(indices) => indices,
        Err(error) => return (0, vec![error]),
    };
    let mut outcomes = Vec::new();
    for index in indices {
        let outcome = match Slot::new(index) {
            Ok(slot) => slot
                .destroy()
                .await
                .map_err(|e| format!("failed to remove network slot {index}: {e}")),
            Err(error) => Err(error),
        };
        outcomes.push(outcome);
    }
    tally(outcomes)
}

pub fn sandbox_cgroups(root: &Path) -> Result<Vec<String>, String> {
    names_in(root, |name| {
        name.len() > SANDBOX_CGROUP_PREFIX.len() && name.starts_with(SANDBOX_CGROUP_PREFIX)
    })
    .map(|names| names.into_iter().filter(|name| root.join(name).is_dir()).collect())
    .map_err(|e| format!("failed to read cgroup root: {e}"))
}

async fn reclaim_cgroups(root: &Path) -> (u64, Vec<String>) {
    let names = match sandbox_cgroups(root) {
        Ok(names) => names,
        Err(error) => return (0, vec![error]),
    };
    let cancel = CancellationToken::new();
    let mut outcomes = Vec::new();
    for name in names {
        outcomes.push(
            Cgroup::existing(root.join(&name))
                .remove(&cancel)
                .await
                .map_err(|e| format!("failed to remove cgroup {name}: {e}")),
        );
    }
    tally(outcomes)
}

pub fn reclaim_files(sandbox_cache_dir: &Path) -> (u64, Vec<String>) {
    let names = match names_in(sandbox_cache_dir, |_| true) {
        Ok(names) => names,
        Err(error) => return (0, vec![error]),
    };
    tally(
        names
            .into_iter()
            .map(|name| sandbox_cache_dir.join(name))
            .map(|path| match remove(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!(
                    "failed to remove {}: {}",
                    path.display(),
                    path_error("remove", &path, &e)
                )),
            })
            .collect(),
    )
}

fn remove(path: &Path) -> std::io::Result<()> {
    match path.is_dir() {
        true => std::fs::remove_dir_all(path),
        false => std::fs::remove_file(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_slot_namespaces_are_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        ["ns-3", "ns-12", "ns-x", "other", "ns-"]
            .iter()
            .for_each(|name| std::fs::write(dir.path().join(name), b"").unwrap());
        std::fs::create_dir(dir.path().join("ns-7")).unwrap();

        assert_eq!(slot_indices(dir.path()).unwrap(), vec![3, 12]);
        assert_eq!(
            slot_indices(Path::new("/definitely/not/netns")).unwrap(),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn only_sandbox_cgroups_are_reclaimed_like_go() {
        let dir = tempfile::tempdir().unwrap();
        ["sbx-a-1", "sbx-b-2", "sbx-", "system", "builder"]
            .iter()
            .for_each(|name| std::fs::create_dir(dir.path().join(name)).unwrap());
        std::fs::write(dir.path().join("sbx-file"), b"").unwrap();

        assert_eq!(sandbox_cgroups(dir.path()).unwrap(), vec!["sbx-a-1", "sbx-b-2"]);
    }

    #[test]
    fn leftover_sandbox_dirs_are_removed_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sandbox-a/nested")).unwrap();
        std::fs::write(dir.path().join("sandbox-a/nested/rootfs.cow"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("sandbox-b")).unwrap();
        std::fs::write(dir.path().join("stray.sock"), b"").unwrap();

        assert_eq!(reclaim_files(dir.path()), (3, Vec::new()));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert_eq!(reclaim_files(&dir.path().join("missing")), (0, Vec::new()));
    }

    #[tokio::test]
    async fn a_live_sandbox_cgroup_is_killed_and_removed() {
        let root = Path::new("/sys/fs/cgroup").join(format!("e2b-reclaim-test-{}", std::process::id()));
        let cgroup = root.join("sbx-leftover");
        if std::fs::create_dir_all(&cgroup).is_err() {
            eprintln!("skipping: needs a writable cgroup v2");
            return;
        }
        let mut sleeper = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), sleeper.id().to_string()).unwrap();

        let outcome = reclaim_cgroups(&root).await;
        let exited = sleeper.wait().unwrap();
        let _ = std::fs::remove_dir(&root);

        assert_eq!(outcome, (1, Vec::new()));
        assert!(!exited.success());
        assert!(!cgroup.exists());
    }

    #[test]
    fn the_summary_counts_reclaims_and_failures() {
        let mut summary = Summary::default();
        summary.record(NETWORK, 2, Vec::new());
        summary.record(
            CGROUP,
            0,
            vec!["failed to remove cgroup sbx-a: device or resource busy".to_owned()],
        );

        assert_eq!(summary.reclaimed, BTreeMap::from([(NETWORK, 2)]));
        assert_eq!(summary.failed, BTreeMap::from([(CGROUP, 1)]));
        assert!(summary.has_failures());
        assert!(!Summary::default().has_failures());
    }
}
