use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::featureflags::{FeatureFlags, STORAGE_CONCURRENT_UPLOAD_LIMIT, STORAGE_MAX_UPLOAD_TASKS};
use crate::server::admission::{AdjustableSemaphore, StartPermit};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const SDK_DEFAULT_CONCURRENCY: usize = 5;
const AZURE_MAX_CONCURRENCY: i64 = u16::MAX as i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadTarget {
    S3,
    Gcs,
    Azure,
    Local,
}

impl UploadTarget {
    pub fn part_concurrency(self, tasks: i64) -> usize {
        match self {
            Self::S3 => sdk_default_when_zero(tasks.max(0)),
            Self::Azure => sdk_default_when_zero(tasks.clamp(0, AZURE_MAX_CONCURRENCY)),
            Self::Gcs | Self::Local => usize::try_from(tasks.max(1)).unwrap_or(usize::MAX),
        }
    }
}

fn sdk_default_when_zero(tasks: i64) -> usize {
    match tasks {
        0 => SDK_DEFAULT_CONCURRENCY,
        tasks => usize::try_from(tasks).unwrap_or(usize::MAX),
    }
}

pub struct UploadLimiter {
    semaphore: Arc<AdjustableSemaphore>,
    flags: FeatureFlags,
    refresh: parking_lot::Mutex<Option<JoinHandle<()>>>,
}

impl UploadLimiter {
    pub fn new(flags: FeatureFlags) -> Result<Arc<Self>, String> {
        AdjustableSemaphore::new(STORAGE_CONCURRENT_UPLOAD_LIMIT.fallback).map(|semaphore| {
            Arc::new(Self {
                semaphore,
                flags,
                refresh: parking_lot::Mutex::new(None),
            })
        })
    }

    pub fn start_refresh(self: &Arc<Self>) {
        let limiter = self.clone();
        self.refresh.lock().get_or_insert_with(|| {
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval_at(tokio::time::Instant::now() + REFRESH_INTERVAL, REFRESH_INTERVAL);
                loop {
                    ticker.tick().await;
                    limiter.refresh_limit();
                }
            })
        });
    }

    fn refresh_limit(&self) {
        let limit = self.flags.get(&STORAGE_CONCURRENT_UPLOAD_LIMIT, &[]);
        if let Err(e) = self.semaphore.set_limit(limit) {
            tracing::error!(error = %e, "failed to adjust upload semaphore");
        }
    }

    pub async fn acquire(&self) -> StartPermit {
        self.semaphore.acquire().await
    }

    pub fn max_upload_tasks(&self) -> i64 {
        self.flags.get(&STORAGE_MAX_UPLOAD_TASKS, &[])
    }
}

impl Drop for UploadLimiter {
    fn drop(&mut self) {
        self.refresh.lock().take().iter().for_each(JoinHandle::abort);
    }
}

pub async fn acquire_upload_slot(limiter: Option<&Arc<UploadLimiter>>) -> Option<StartPermit> {
    match limiter {
        Some(limiter) => Some(limiter.acquire().await),
        None => None,
    }
}

pub fn max_upload_tasks(limiter: Option<&Arc<UploadLimiter>>) -> i64 {
    limiter.map_or(STORAGE_MAX_UPLOAD_TASKS.fallback, |limiter| limiter.max_upload_tasks())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_keeps_the_sdk_default_for_zero_and_negative_values() {
        assert_eq!(UploadTarget::S3.part_concurrency(-1), SDK_DEFAULT_CONCURRENCY);
        assert_eq!(UploadTarget::S3.part_concurrency(0), SDK_DEFAULT_CONCURRENCY);
        assert_eq!(UploadTarget::S3.part_concurrency(16), 16);
    }

    #[test]
    fn gcs_always_keeps_at_least_one_uploader() {
        assert_eq!(UploadTarget::Gcs.part_concurrency(-3), 1);
        assert_eq!(UploadTarget::Gcs.part_concurrency(0), 1);
        assert_eq!(UploadTarget::Gcs.part_concurrency(7), 7);
    }

    #[test]
    fn azure_clamps_to_what_the_sdk_field_can_express() {
        assert_eq!(UploadTarget::Azure.part_concurrency(-1), SDK_DEFAULT_CONCURRENCY);
        assert_eq!(UploadTarget::Azure.part_concurrency(1 << 40), usize::from(u16::MAX));
        assert_eq!(UploadTarget::Azure.part_concurrency(12), 12);
    }

    #[tokio::test]
    async fn without_a_limiter_an_upload_is_not_throttled_and_uses_the_flag_fallback() {
        assert!(acquire_upload_slot(None).await.is_none());
        assert_eq!(max_upload_tasks(None), 16);
    }

    #[tokio::test]
    async fn the_slot_blocks_when_full_and_frees_on_release() {
        let limiter = UploadLimiter::new(FeatureFlags::offline()).unwrap();
        limiter.semaphore.set_limit(1).unwrap();

        let held = limiter.acquire().await;
        let blocked = tokio::time::timeout(Duration::from_millis(20), limiter.acquire()).await;
        assert!(blocked.is_err());

        drop(held);
        drop(limiter.acquire().await);
    }

    #[test]
    fn the_flags_serve_the_upstream_keys_and_fallbacks() {
        assert_eq!(
            (
                STORAGE_CONCURRENT_UPLOAD_LIMIT.key,
                STORAGE_CONCURRENT_UPLOAD_LIMIT.fallback
            ),
            ("gcloud-concurrent-upload-limit", 8)
        );
        assert_eq!(
            (STORAGE_MAX_UPLOAD_TASKS.key, STORAGE_MAX_UPLOAD_TASKS.fallback),
            ("gcloud-max-tasks", 16)
        );
    }

    #[tokio::test]
    async fn the_refresh_applies_the_flag_to_the_semaphore() {
        let flags = FeatureFlags::builder()
            .override_flag("gcloud-concurrent-upload-limit", 3)
            .offline();
        let limiter = UploadLimiter::new(flags).unwrap();

        limiter.refresh_limit();

        assert_eq!(limiter.semaphore.limit(), 3);
    }
}
