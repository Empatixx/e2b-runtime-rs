use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tonic::Status;

use super::Node;
use super::SANDBOX_TYPE;
use super::prefetch::pause_contexts;
use crate::block::build_file::Ready;
use crate::featureflags::{FlagContext, memfile_diff_dedup, pause_admission_disk_headroom_mib};
use crate::firecracker::version::supports_memfd;
use crate::memory::dedup::config::DedupConfig;
use crate::proto::SandboxConfig;
use crate::sandbox_logs::{SandboxMeta, external};
use crate::server::pause_disk::{CaptureSize, DiskShort, MIB, PauseDiskReservation, rootfs_cache_bytes};
use crate::server::registry::Lifecycle;
use crate::telemetry::server::sandbox::record_pause_admission;

pub const ROOTFS_OVERLAY_FILE: &str = "rootfs.overlay";
const OUTCOME_REFUSED_DISK: &str = "refused_disk";

impl Node {
    pub fn rootfs_cache_of(&self, lifecycle: &Lifecycle) -> PathBuf {
        self.work_dir_of(lifecycle).join(ROOTFS_OVERLAY_FILE)
    }

    fn dedup_keeps_an_intermediate_copy(&self, config: &SandboxConfig) -> bool {
        let dedup = DedupConfig::from_flag(&self.feature_flags.get(&memfile_diff_dedup(), &pause_contexts(config)));
        let memfd = supports_memfd(&config.firecracker_version)
            && self.runtime_flags.use_memfd(FlagContext::sandbox_runtime(
                &config.sandbox_id,
                &config.template_id,
                &config.kernel_version,
                &config.firecracker_version,
                &config.envd_version,
                SANDBOX_TYPE,
            ));
        dedup.enabled && !memfd
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
        config: &SandboxConfig,
        rootfs_cache: &Path,
        filesystem_only: bool,
    ) -> Result<Option<Arc<PauseDiskReservation>>, Status> {
        let headroom_mib = self.feature_flags.get(&pause_admission_disk_headroom_mib(), &[]);
        if headroom_mib < 0 {
            return Ok(None);
        }
        let sandbox_id = config.sandbox_id.as_str();
        let Ok(rootfs_cache_bytes) = rootfs_cache_bytes(rootfs_cache).inspect_err(|e| {
            tracing::warn!(sandbox = %sandbox_id, error = %e, "Cannot estimate the snapshot's size, admitting the pause");
        }) else {
            return Ok(None);
        };
        let size = CaptureSize {
            ram_bytes: config.ram_mb << 20,
            rootfs_cache_bytes,
            filesystem_only,
            dedup_intermediate_copy: self.dedup_keeps_an_intermediate_copy(config),
        };
        self.build_disk
            .admit(headroom_mib, &size)
            .map(|reservation| reservation.map(Arc::new))
            .map_err(|short| refused(config, &short))
    }
}

fn refused(config: &SandboxConfig, short: &DiskShort) -> Status {
    let sandbox_id = &config.sandbox_id;
    record_pause_admission(&config.team_id, "pause", OUTCOME_REFUSED_DISK, Duration::ZERO);
    external(&SandboxMeta::of(config)).warn(
        "Refusing pause: the node is short of disk for the snapshot, retry later",
        Vec::new(),
    );
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
