use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use uuid::Uuid;

use super::deferred_memory::{Begin, Deferred, End, MemfdSource, Pending, Setup, setup};
use super::export_union::ExportBaseline;
use super::fpr_resume::FprResume;
use super::{Freeze, InPlaceFailure, LostSandbox, SANDBOX_LOST, record_freeze_for};
use crate::block::build_file::{Ready, ready_now};
use crate::memory::dedup::config::DedupConfig;
use crate::memory::snapshot::diff_metadata;
use crate::sandbox::Sandbox;
use crate::sandbox::snapshot::{Exported, LocalSnapshot, MemoryPages, Snapshot, SnapshotFiles, exported};
use crate::storage::build::Build;
use crate::telemetry::server::in_place::ProcessMemoryOutcome;
use crate::telemetry::snapshot_diff::{record_snapshot_dedup, record_snapshot_diff};

#[derive(Default)]
pub struct MemoryExport {
    pub baseline: ExportBaseline,
    seal: Mutex<Option<Ready>>,
    fpr_generation: Arc<AtomicU64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCheckpointRequest {
    pub dir: PathBuf,
    pub build_id: Uuid,
    pub team_id: String,
    pub balloon_mode: String,
    pub defer_export: bool,
    pub tracker_dirty: bool,
}

pub struct MemoryCheckpoint {
    pub snapshot: LocalSnapshot,
    pub deferred: bool,
}

struct ExportFailure {
    error: String,
    pending: Option<Pending>,
}

fn failed(error: String) -> Box<ExportFailure> {
    Box::new(ExportFailure { error, pending: None })
}

struct ReportingHold {
    paused: bool,
}

struct Exports {
    memfile: Exported,
    memory_bytes: u64,
    tracked_writes: bool,
    ready: Ready,
    pending: Option<Pending>,
    rootfs: Exported,
}

impl Sandbox {
    pub async fn checkpoint_memory_in_place(
        &self,
        request: &MemoryCheckpointRequest,
    ) -> Result<MemoryCheckpoint, InPlaceFailure> {
        self.wait_for_memory_seal()
            .await
            .map_err(|e| InPlaceFailure::alive(format!("previous memory capture did not complete: {e}")))?;
        let hold = self.hold_reporting(request.defer_export).await;
        let mut freeze = Freeze {
            started: Instant::now(),
            paused: false,
        };
        let paused = self.pause_vm().await;
        let exported = match paused {
            Ok(()) => {
                freeze.paused = true;
                self.export_for_checkpoint(request, &hold).await
            }
            Err(e) => Err(failed(e)),
        };
        match exported {
            Ok(exports) => self.resume_after_memory_snapshot(request, freeze, hold, exports).await,
            Err(failure) => Err(self
                .resume_after_memory_failure(request, freeze, hold, failure.pending, failure.error)
                .await),
        }
    }

    pub async fn wait_for_memory_seal(&self) -> Result<(), String> {
        let seal = self.memory_export.seal.lock().clone();
        match seal {
            Some(seal) => seal.await,
            None => Ok(()),
        }
    }

    async fn hold_reporting(&self, defer_export: bool) -> ReportingHold {
        let not_held = ReportingHold { paused: false };
        if !defer_export {
            return not_held;
        }
        let caps = match self.balloon.caps().await {
            Ok(caps) => caps,
            Err(e) => {
                tracing::warn!(error = %e, "defer-memory-export: balloon query failed; using sync copy");
                return not_held;
            }
        };
        if !caps.reporting {
            return not_held;
        }
        let Some(client) = self.resources.vm.as_ref().map(|vm| vm.client()) else {
            return not_held;
        };
        if let Err(e) = client.pause_free_page_reporting().await {
            tracing::warn!(error = %e, "defer-memory-export: pausing free-page reporting failed; using sync copy");
            return not_held;
        }
        match client.free_page_reporting_paused().await {
            Ok(true) => {
                self.memory_export.fpr_generation.fetch_add(1, Ordering::SeqCst);
                ReportingHold { paused: true }
            }
            confirmed => {
                tracing::warn!(paused = ?confirmed, "defer-memory-export: free-page reporting pause not confirmed; using sync copy");
                self.fpr_resume().run().await;
                not_held
            }
        }
    }

    pub(super) fn fpr_resume(&self) -> FprResume {
        let client = Arc::new(self.resources.vm.as_ref().map(|vm| vm.client()));
        let pid = self.resources.vm.as_ref().and_then(|vm| vm.pid());
        let generation = self.memory_export.fpr_generation.clone();
        FprResume::production(
            Arc::new(move || {
                let client = client.clone();
                Box::pin(async move {
                    match client.as_ref() {
                        Some(client) => client.resume_free_page_reporting().await,
                        None => Err("firecracker is not running".to_owned()),
                    }
                })
            }),
            Arc::new(move || pid.is_none_or(|pid| !process_alive(pid))),
            Arc::new(move || generation.load(Ordering::SeqCst)),
        )
    }

    async fn export_for_checkpoint(
        &self,
        request: &MemoryCheckpointRequest,
        hold: &ReportingHold,
    ) -> Result<Exports, Box<ExportFailure>> {
        let started = Instant::now();
        let client = self
            .resources
            .vm
            .as_ref()
            .ok_or_else(|| failed("firecracker is not running".to_owned()))?
            .client();
        std::fs::create_dir_all(&request.dir).map_err(|e| failed(format!("create {}: {e}", request.dir.display())))?;
        let files = SnapshotFiles::in_dir(&request.dir);
        client
            .create_snapshot(&files.snapfile)
            .await
            .map_err(|e| failed(format!("error creating snapshot: {e}")))?;
        let mut pages = match request.tracker_dirty {
            true => self.tracker_pages(&client).await,
            false => self.memory_pages(&client).await,
        }
        .map_err(|e| failed(format!("failed to get memfile metadata: {e}")))?;
        self.memory_export
            .baseline
            .apply(&mut pages.dirty, &mut pages.empty, true);
        let memory_bytes = pages.regions.iter().map(|region| region.size).sum();
        let tracked_writes = pages.tracked_writes;
        let page_size = pages
            .regions
            .first()
            .map(|region| region.page_size)
            .ok_or_else(|| failed("firecracker reported no memory regions".to_owned()))?;
        let parent = self.memory_parent(page_size, memory_bytes).map_err(failed)?;
        let deferred = self.defer_memory(request, hold, &pages, page_size, &files);
        let (memfile, ready, pending, was_deferred) = match deferred {
            Some(Ok(Deferred {
                ready,
                pending,
                occupancy,
            })) => {
                *self.memory_export.seal.lock() = Some(occupancy);
                let diff =
                    diff_metadata(&pages.regions, &pages.dirty, &pages.empty).map_err(|e| failed(e.to_string()))?;
                record_snapshot_diff(&request.team_id, "memfile", &diff, &parent);
                record_snapshot_dedup(&request.team_id, "memfile", &diff, None, false);
                let memfile = exported(&diff, &parent, request.build_id).map_err(failed)?;
                (memfile, ready, Some(pending), true)
            }
            other => {
                if let Some(Err(e)) = other {
                    tracing::error!(error = %e, "defer-memory-export setup failed; using sync copy");
                }
                let pid = self.resources.vm.as_ref().and_then(|vm| vm.pid()).unwrap_or(0);
                let (memfile, ready) = self
                    .export_memory(pages, pid, &files.memfile, request.build_id, DedupConfig::default())
                    .await
                    .map_err(failed)?;
                ready
                    .clone()
                    .await
                    .map_err(|e| failed(format!("synchronous in-place memory export failed: {e}")))?;
                if hold.paused {
                    self.fpr_resume().run().await;
                }
                (memfile, ready_now(), None, false)
            }
        };
        ProcessMemoryOutcome {
            in_place: true,
            deferred: was_deferred,
            balloon_mode: request.balloon_mode.clone(),
            success: true,
            team_id: request.team_id.clone(),
        }
        .record(started.elapsed());
        let rootfs = match self.export_disk(&files.rootfs, request.build_id).await {
            Ok(rootfs) => rootfs,
            Err(e) => {
                return Err(Box::new(ExportFailure {
                    error: format!("error while post processing: {e}"),
                    pending,
                }));
            }
        };
        Ok(Exports {
            memfile,
            memory_bytes,
            tracked_writes,
            ready,
            pending,
            rootfs,
        })
    }

    fn defer_memory(
        &self,
        request: &MemoryCheckpointRequest,
        hold: &ReportingHold,
        pages: &MemoryPages,
        page_size: u64,
        files: &SnapshotFiles,
    ) -> Option<Result<Deferred, String>> {
        if !request.defer_export || pages.dirty.is_empty() {
            return None;
        }
        let memfd = self.resources.memory.as_ref().and_then(|memory| memory.memfd())?;
        let exporter = self.resources.memory.as_ref()?.exporter();
        let begin: Begin = {
            let exporter = exporter.clone();
            Box::new(move |dirty, source, sink| exporter.begin(dirty, source, sink))
        };
        let end: End = Arc::new(move |window| {
            let exporter = exporter.clone();
            Box::pin(async move { exporter.end(&window).await })
        });
        Some(setup(Setup {
            dirty: &pages.dirty,
            page_size,
            path: &files.memfile,
            source: Arc::new(MemfdSource::new(memfd)),
            begin,
            end,
            fpr: hold.paused.then(|| self.fpr_resume()),
            team_id: &request.team_id,
        }))
    }

    async fn resume_after_memory_snapshot(
        &self,
        request: &MemoryCheckpointRequest,
        freeze: Freeze,
        _hold: ReportingHold,
        exports: Exports,
    ) -> Result<MemoryCheckpoint, InPlaceFailure> {
        let deferred = exports.pending.is_some();
        if let Err(e) = self.resume_vm().await {
            if let Some(pending) = exports.pending {
                pending.abort().await;
            }
            return Err(InPlaceFailure::lost(
                LostSandbox::ResumeFailed,
                format!("resume in place failed, sandbox torn down: {SANDBOX_LOST}\n{e}"),
            ));
        }
        record_freeze_for(&request.team_id, &request.balloon_mode, deferred, &freeze, true);
        self.reinit_envd();
        if let Some(pending) = exports.pending {
            pending.start();
        }
        let files = SnapshotFiles::in_dir(&request.dir);
        Ok(MemoryCheckpoint {
            snapshot: LocalSnapshot {
                snapshot: Snapshot {
                    build: Build {
                        build_id: request.build_id,
                        memfile: exports.memfile.header,
                        rootfs: exports.rootfs.header,
                    },
                    memfile_bytes: exports.memfile.bytes,
                    memory_bytes: exports.memory_bytes,
                    rootfs_bytes: exports.rootfs.bytes,
                    tracked_writes: exports.tracked_writes,
                    vm_vcpus: self.resolved_vm_vcpus,
                },
                files,
                memfile_ready: exports.ready,
            },
            deferred,
        })
    }

    async fn resume_after_memory_failure(
        &self,
        request: &MemoryCheckpointRequest,
        freeze: Freeze,
        hold: ReportingHold,
        pending: Option<Pending>,
        error: String,
    ) -> InPlaceFailure {
        let owned_by_window = pending.is_some();
        if let Some(pending) = pending {
            pending.abort().await;
        }
        if hold.paused && !owned_by_window {
            self.fpr_resume().run().await;
        }
        if let Err(e) = self.resume_vm().await {
            return InPlaceFailure::lost(
                LostSandbox::ResumeFailed,
                format!("resume in place failed during pause cleanup, sandbox torn down: {SANDBOX_LOST}\n{e}"),
            );
        }
        if freeze.paused {
            record_freeze_for(&request.team_id, &request.balloon_mode, false, &freeze, false);
            self.reinit_envd();
        }
        InPlaceFailure::alive(error)
    }
}

fn process_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}
