use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::logs::BuildLog;
use crate::proto::{TemplateBuildMetadata, TemplateBuildState, TemplateBuildStatusReason};

pub const BUILD_EXPIRATION: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq)]
pub struct BuildResult {
    pub status: TemplateBuildState,
    pub reason: Option<TemplateBuildStatusReason>,
    pub metadata: Option<TemplateBuildMetadata>,
}

pub struct BuildInfo {
    pub log: BuildLog,
    pub cancel: CancellationToken,
    result: watch::Sender<Option<BuildResult>>,
}

impl BuildInfo {
    pub fn result(&self) -> Option<BuildResult> {
        self.result.borrow().clone()
    }

    pub fn finish(&self, result: BuildResult) {
        self.result.send_if_modified(|current| match current {
            Some(_) => false,
            None => {
                *current = Some(result);
                true
            }
        });
    }

    pub async fn finished(&self) -> BuildResult {
        let mut receiver = self.result.subscribe();
        receiver
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|result| result.clone())
            .unwrap_or(BuildResult {
                status: TemplateBuildState::Failed,
                reason: None,
                metadata: None,
            })
    }
}

#[derive(Default)]
pub struct BuildCache {
    builds: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    info: Arc<BuildInfo>,
    finished_at: Option<Instant>,
}

impl BuildCache {
    pub fn register(&self, build_id: &str) -> Result<Arc<BuildInfo>, String> {
        let mut builds = self.builds.lock();
        if builds.contains_key(build_id) {
            return Err(format!("build {build_id} already exists in cache"));
        }
        let info = Arc::new(BuildInfo {
            log: BuildLog::default(),
            cancel: CancellationToken::new(),
            result: watch::channel(None).0,
        });
        builds.insert(
            build_id.to_owned(),
            Entry {
                info: info.clone(),
                finished_at: None,
            },
        );
        Ok(info)
    }

    pub fn get(&self, build_id: &str) -> Result<Arc<BuildInfo>, String> {
        self.builds
            .lock()
            .get(build_id)
            .map(|entry| entry.info.clone())
            .ok_or_else(|| format!("build {build_id} not found in cache"))
    }

    pub fn running(&self) -> Vec<Arc<BuildInfo>> {
        self.builds
            .lock()
            .values()
            .filter(|entry| entry.info.result().is_none())
            .map(|entry| entry.info.clone())
            .collect()
    }

    pub fn evict_expired(&self) {
        let now = Instant::now();
        let mut builds = self.builds.lock();
        builds
            .values_mut()
            .filter(|entry| entry.finished_at.is_none() && entry.info.result().is_some())
            .for_each(|entry| entry.finished_at = Some(now));
        builds.retain(|_, entry| {
            entry
                .finished_at
                .is_none_or(|finished| now < finished + BUILD_EXPIRATION)
        });
    }

    pub async fn run_eviction(self: Arc<Self>) {
        let mut ticks = tokio::time::interval(Duration::from_secs(1));
        loop {
            ticks.tick().await;
            self.evict_expired();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed() -> BuildResult {
        BuildResult {
            status: TemplateBuildState::Completed,
            reason: None,
            metadata: None,
        }
    }

    #[test]
    fn a_build_id_registers_once() {
        let cache = BuildCache::default();
        cache.register("b1").unwrap();

        assert_eq!(
            cache.register("b1").err(),
            Some("build b1 already exists in cache".to_owned())
        );
        assert_eq!(cache.get("b2").err(), Some("build b2 not found in cache".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_finished_build_expires_after_ten_minutes_and_the_first_result_wins() {
        let cache = BuildCache::default();
        let info = cache.register("b1").unwrap();
        info.finish(completed());
        info.finish(BuildResult {
            status: TemplateBuildState::Failed,
            ..completed()
        });
        assert_eq!(info.finished().await, completed());

        cache.evict_expired();
        tokio::time::advance(BUILD_EXPIRATION - Duration::from_secs(1)).await;
        cache.evict_expired();
        assert!(cache.get("b1").is_ok());
        tokio::time::advance(Duration::from_secs(2)).await;
        cache.evict_expired();
        assert!(cache.get("b1").is_err());
    }
}
