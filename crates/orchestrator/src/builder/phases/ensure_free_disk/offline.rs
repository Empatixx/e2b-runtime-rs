use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use super::deadline::{DEADLINE_EXCEEDED, Deadline};
use crate::block::overlay::Overlay;
use crate::builder::ext4::Ext4;
use crate::exposure::nbd::{Exposure, expose};

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(60);

pub struct OfflineDevice {
    exposure: Exposure,
}

impl OfflineDevice {
    async fn open(overlay: Arc<Overlay>) -> Result<Self, String> {
        expose(overlay)
            .await
            .map(|exposure| Self { exposure })
            .map_err(|e| format!("open NBD device: {e}"))
    }

    pub fn ext4(&self) -> Ext4 {
        Ext4::new(&self.exposure.path())
    }

    pub async fn flush(&self) -> Result<(), String> {
        self.exposure.flush().await
    }

    async fn close(self) -> Result<(), String> {
        self.exposure.close().await
    }
}

pub async fn with_device<T>(
    overlay: Arc<Overlay>,
    deadline: Deadline,
    work: impl AsyncFnOnce(&OfflineDevice) -> Result<T, String>,
) -> Result<T, String> {
    let device = deadline.bound(OfflineDevice::open(overlay)).await?;
    let worked = deadline.bound(work(&device)).await;
    close_after(worked, device.close(), CLEANUP_TIMEOUT).await
}

async fn close_after<T>(
    worked: Result<T, String>,
    close: impl Future<Output = Result<(), String>>,
    cleanup_timeout: Duration,
) -> Result<T, String> {
    let closed = tokio::time::timeout(cleanup_timeout, close)
        .await
        .unwrap_or_else(|_| Err(DEADLINE_EXCEEDED.to_owned()));
    join_failures(worked, closed)
}

fn join_failures<T>(worked: Result<T, String>, closed: Result<(), String>) -> Result<T, String> {
    match (worked, closed) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(closing)) => Err(closing),
        (Err(working), Ok(())) => Err(working),
        (Err(working), Err(closing)) => Err(format!("{working}\n{closing}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_device_is_closed_even_when_the_work_hit_its_deadline() {
        let closed = std::sync::atomic::AtomicBool::new(false);
        let worked = Deadline::after(Duration::from_secs(1))
            .bound(async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(())
            })
            .await;

        let result = close_after(
            worked,
            async {
                closed.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            Duration::from_secs(60),
        )
        .await;

        assert_eq!(result, Err("context deadline exceeded".to_owned()));
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_that_hangs_is_cut_off_after_the_cleanup_timeout() {
        let result = close_after(Ok(7), std::future::pending(), Duration::from_secs(60)).await;

        assert_eq!(result, Err("context deadline exceeded".to_owned()));
    }

    #[test]
    fn a_close_failure_joins_the_work_failure_like_go_errors_join() {
        assert_eq!(join_failures(Ok(3), Ok(())), Ok(3));
        assert_eq!(join_failures(Ok(3), Err("close".to_owned())), Err("close".to_owned()));
        assert_eq!(
            join_failures::<i32>(Err("work".to_owned()), Ok(())),
            Err("work".to_owned())
        );
        assert_eq!(
            join_failures::<i32>(Err("work".to_owned()), Err("close".to_owned())),
            Err("work\nclose".to_owned())
        );
    }
}
