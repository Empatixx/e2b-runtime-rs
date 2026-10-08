use std::sync::Arc;

use crate::block::overlay::Overlay;
use crate::builder::ext4::Ext4;
use crate::exposure::nbd::{Exposure, expose};

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
    work: impl AsyncFnOnce(&OfflineDevice) -> Result<T, String>,
) -> Result<T, String> {
    let device = OfflineDevice::open(overlay).await?;
    let worked = work(&device).await;
    let closed = device.close().await;
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
