use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache;
use tokio::sync::watch;
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

pub struct BuildCache {
    builds: Cache<String, Arc<BuildInfo>>,
}

impl Default for BuildCache {
    fn default() -> Self {
        Self {
            builds: Cache::builder().time_to_idle(BUILD_EXPIRATION).build(),
        }
    }
}

impl BuildCache {
    pub fn register(&self, build_id: &str) -> Result<Arc<BuildInfo>, String> {
        let entry = self.builds.entry_by_ref(build_id).or_insert_with(|| {
            Arc::new(BuildInfo {
                log: BuildLog::default(),
                cancel: CancellationToken::new(),
                result: watch::Sender::new(None),
            })
        });
        entry
            .is_fresh()
            .then(|| entry.into_value())
            .ok_or_else(|| format!("build {build_id} already exists in cache"))
    }

    pub fn get(&self, build_id: &str) -> Result<Arc<BuildInfo>, String> {
        self.builds
            .get(build_id)
            .ok_or_else(|| format!("build {build_id} not found in cache"))
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

    #[tokio::test]
    async fn the_first_result_wins() {
        let cache = BuildCache::default();
        let info = cache.register("b1").unwrap();
        info.finish(completed());
        info.finish(BuildResult {
            status: TemplateBuildState::Failed,
            ..completed()
        });
        assert_eq!(info.finished().await, completed());
        assert!(Arc::ptr_eq(&info, &cache.get("b1").unwrap()));
    }
}
