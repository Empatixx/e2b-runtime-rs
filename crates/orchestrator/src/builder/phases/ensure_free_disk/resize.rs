use std::sync::Arc;

use super::deadline::Deadline;
use super::offline::with_device;
use crate::block::overlay::Overlay;

pub async fn resize_offline(
    overlay: Arc<Overlay>,
    new_size: i64,
    block_size: i64,
    deadline: Deadline,
) -> Result<i64, String> {
    with_device(overlay, deadline, async |device| {
        let ext4 = device.ext4();
        ext4.check_integrity(true)
            .await
            .map_err(|e| format!("pre-resize e2fsck: {e}"))?;
        ext4.resize(new_size).await.map_err(|e| format!("resize rootfs: {e}"))?;
        ext4.check_integrity(true)
            .await
            .map_err(|e| format!("post-resize e2fsck: {e}"))?;
        let free_after = ext4
            .free_space(block_size)
            .await
            .map_err(|e| format!("measure grown free space: {e}"))?;
        device.flush().await.map_err(|e| format!("flush grown device: {e}"))?;
        Ok(free_after)
    })
    .await
}
