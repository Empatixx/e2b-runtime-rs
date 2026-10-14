use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::Layers;

const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);
const START_AT: f64 = 0.95;
const EVICT_DOWN_TO: f64 = 0.70;
const MIN_IDLE: Duration = Duration::from_secs(10 * 60);

struct CachedFile {
    path: PathBuf,
    size: u64,
    used: SystemTime,
}

pub fn spawn_eviction(layers: Arc<Layers>, capacity_bytes: u64) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHECK_EVERY).await;
            let freed = evict(layers.dir(), capacity_bytes);
            if freed > 0 {
                tracing::info!(freed_bytes = freed, "evicted cached layers");
            }
        }
    });
}

pub fn evict(dir: &Path, capacity_bytes: u64) -> u64 {
    let mut files = cached_files(dir);
    let mut total: u64 = files.iter().map(|file| file.size).sum();
    if (total as f64) < capacity_bytes as f64 * START_AT {
        return 0;
    }
    files.sort_by_key(|file| file.used);
    let target = (capacity_bytes as f64 * EVICT_DOWN_TO) as u64;
    let idle_before = SystemTime::now() - MIN_IDLE;
    let mut freed = 0;
    for file in files.iter().filter(|file| file.used < idle_before) {
        if total <= target {
            break;
        }
        if std::fs::remove_file(&file.path).is_ok() {
            total -= file.size;
            freed += file.size;
        }
    }
    freed
}

fn cached_files(dir: &Path) -> Vec<CachedFile> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            Some(CachedFile {
                path: entry.path(),
                size: metadata.len(),
                used: metadata.modified().ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(dir: &Path, name: &str, size: usize, age: Duration) {
        let path = dir.join(name);
        std::fs::write(&path, vec![0u8; size]).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn evicts_the_oldest_idle_layers_down_to_the_low_mark() {
        let dir = tempfile::tempdir().unwrap();
        cached(dir.path(), "old", 400, Duration::from_secs(3600));
        cached(dir.path(), "middle", 400, Duration::from_secs(1800));
        cached(dir.path(), "fresh", 200, Duration::from_secs(1));
        assert_eq!(evict(dir.path(), 1000), 400);
        assert!(!dir.path().join("old").exists());
        assert!(dir.path().join("middle").exists());
    }

    #[test]
    fn keeps_everything_below_the_high_mark() {
        let dir = tempfile::tempdir().unwrap();
        cached(dir.path(), "old", 400, Duration::from_secs(3600));
        assert_eq!(evict(dir.path(), 1000), 0);
    }
}
