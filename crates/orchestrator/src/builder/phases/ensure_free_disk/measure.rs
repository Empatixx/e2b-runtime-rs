use std::path::Path;
use std::sync::Arc;

use super::deadline::Deadline;
use super::offline::with_device;
use crate::block::build_file::BuildFile;
use crate::block::overlay::Overlay;

pub async fn measure_free(
    source: &Arc<BuildFile>,
    cache_path: &Path,
    block_size: i64,
    deadline: Deadline,
) -> Result<i64, String> {
    let overlay = Overlay::new(source.clone(), cache_path)
        .map(Arc::new)
        .map_err(|e| format!("create measurement cache: {e}"))?;
    with_device(overlay, deadline, async |device| {
        let ext4 = device.ext4();
        ext4.replay_journal()
            .await
            .map_err(|e| format!("replay source journal: {e}"))?;
        device
            .flush()
            .await
            .map_err(|e| format!("flush recovered source journal: {e}"))?;
        ext4.free_space(block_size)
            .await
            .map_err(|e| format!("measure source free space: {e}"))
    })
    .await
}
