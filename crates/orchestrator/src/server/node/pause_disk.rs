use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tonic::Status;

use super::Node;
use crate::block::build_file::Ready;
use crate::featureflags::pause_admission_disk_headroom_mib;
use crate::server::pause_disk::{CaptureSize, DiskShort, MIB, PauseDiskReservation, rootfs_cache_bytes};
use crate::server::registry::Lifecycle;
use crate::telemetry::server::sandbox::record_pause_admission;

pub const ROOTFS_OVERLAY_FILE: &str = "rootfs.overlay";
const OUTCOME_REFUSED_DISK: &str = "refused_disk";

impl Node {
    pub fn rootfs_cache_of(&self, lifecycle: &Lifecycle) -> PathBuf {
        self.work_dir_of(lifecycle).join(ROOTFS_OVERLAY_FILE)
    }

    pub fn mark_written_when_ready(&self, reservation: Option<Arc<PauseDiskReservation>>, ready: Ready) {
        let work = self.status.track_work();
        if let Some(reservation) = reservation {
            tokio::spawn(async move {
                drop(ready.await);
                reservation.written();
                drop(work);
            });
        }
    }

    pub fn admit_pause_disk(
        &self,
        sandbox_id: &str,
        team_id: &str,
        ram_mb: i64,
        rootfs_cache: &Path,
        filesystem_only: bool,
    ) -> Result<Option<Arc<PauseDiskReservation>>, Status> {
        let headroom_mib = self.feature_flags.get(&pause_admission_disk_headroom_mib(), &[]);
        if headroom_mib < 0 {
            return Ok(None);
        }
        let Ok(rootfs_cache_bytes) = rootfs_cache_bytes(rootfs_cache).inspect_err(|e| {
            tracing::warn!(sandbox = %sandbox_id, error = %e, "Cannot estimate the snapshot's size, admitting the pause");
        }) else {
            return Ok(None);
        };
        let size = CaptureSize {
            ram_bytes: ram_mb << 20,
            rootfs_cache_bytes,
            filesystem_only,
        };
        self.build_disk
            .admit(headroom_mib, &size)
            .map(|reservation| reservation.map(Arc::new))
            .map_err(|short| refused(sandbox_id, team_id, &short))
    }
}

fn refused(sandbox_id: &str, team_id: &str, short: &DiskShort) -> Status {
    record_pause_admission(team_id, "pause", OUTCOME_REFUSED_DISK, Duration::ZERO);
    tracing::warn!(
        sandbox = %sandbox_id,
        need_bytes = short.need,
        available_bytes = short.available,
        reserved_bytes = short.reserved,
        headroom_bytes = short.headroom,
        "Refusing pause: the build filesystem cannot hold the snapshot"
    );
    Status::resource_exhausted(format!(
        "node is short of disk for the snapshot of sandbox '{sandbox_id}' ({} MiB needed, {} MiB free), please retry",
        short.need / MIB,
        (short.available - short.reserved) / MIB
    ))
}
