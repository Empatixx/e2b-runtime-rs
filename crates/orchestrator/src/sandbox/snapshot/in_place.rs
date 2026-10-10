mod deferred_memory;
mod export_union;
mod fpr_resume;
pub mod memory;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Exported, SnapshotFiles};
use crate::block::build_file::Ready;
use crate::envd::quiesce::guest::GuestEnvd;
use crate::envd::quiesce::{GuestQuiesce, Preparation, PreparationFailure};
use crate::envd::reclaim::GuestReclaim;
use crate::envd::{self};
use crate::sandbox::Sandbox;
use crate::storage::build::Build;
use crate::telemetry::server::in_place::{
    GuestFreezeOutcome, GuestSyncOutcome, ProcessRootfsOutcome, count_fs_quiesced_pause, record_fsthaw,
};

const STATE_FLIP_TIMEOUT: Duration = Duration::from_secs(40);
const SANDBOX_LOST: &str = "sandbox lost";
const THAW_FAILED: &str = "rootfs thaw failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuiesceFlags {
    pub sync_timeout_override_ms: i64,
    pub freeze_via_exec: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemCheckpointRequest {
    pub dir: PathBuf,
    pub build_id: Uuid,
    pub flags: QuiesceFlags,
    pub team_id: String,
    pub balloon_mode: String,
    pub defer_rootfs: bool,
}

#[derive(Clone)]
pub struct FilesystemCheckpoint {
    pub build: Build,
    pub rootfs_diff: PathBuf,
    pub rootfs_bytes: u64,
    pub frozen: bool,
    pub vm_vcpus: i64,
    pub rootfs_sealed: Ready,
}

impl std::fmt::Debug for FilesystemCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemCheckpoint")
            .field("build", &self.build)
            .field("rootfs_diff", &self.rootfs_diff)
            .field("rootfs_bytes", &self.rootfs_bytes)
            .field("frozen", &self.frozen)
            .field("vm_vcpus", &self.vm_vcpus)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostSandbox {
    ResumeFailed,
    ThawFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InPlaceFailure {
    pub message: String,
    pub lost: Option<LostSandbox>,
}

impl InPlaceFailure {
    fn alive(message: String) -> Self {
        Self { message, lost: None }
    }

    fn lost(lost: LostSandbox, message: String) -> Self {
        Self {
            message,
            lost: Some(lost),
        }
    }
}

struct Freeze {
    started: Instant,
    paused: bool,
}

impl Sandbox {
    pub fn guest_reclaim(&self) -> GuestReclaim<'_> {
        GuestReclaim {
            envd: GuestEnvd {
                base_url: format!("http://{}", self.envd_address()),
                access_token: self.config.envd.access_token.clone(),
            },
            envd_version: &self.config.envd_version,
            sandbox_id: &self.config.id,
            team_id: &self.config.team_id,
        }
    }

    pub fn guest_quiesce(&self, flags: QuiesceFlags) -> GuestQuiesce {
        GuestQuiesce::builder()
            .base_url(format!("http://{}", self.envd_address()))
            .maybe_access_token(self.config.envd.access_token.clone())
            .envd_version(self.config.envd_version.as_str())
            .ram_mb(i64::from(self.config.memory_mib))
            .sync_timeout_override_ms(flags.sync_timeout_override_ms)
            .freeze_via_exec(flags.freeze_via_exec)
            .build()
    }

    pub async fn checkpoint_filesystem_in_place(
        &self,
        request: &FilesystemCheckpointRequest,
    ) -> Result<FilesystemCheckpoint, InPlaceFailure> {
        self.wait_for_rootfs_seal().await.map_err(InPlaceFailure::alive)?;
        let quiesce = self.guest_quiesce(request.flags);
        let prepared = self.quiesce_guest(&quiesce, request).await?;
        let mut freeze = Freeze {
            started: Instant::now(),
            paused: false,
        };
        let exported = match self.pause_vm().await {
            Ok(()) => {
                freeze.paused = true;
                self.export_filesystem(request, prepared.frozen, true).await
            }
            Err(e) => Err(e),
        };
        match exported {
            Ok(checkpoint) => self.resume_after_snapshot(&quiesce, request, freeze, checkpoint).await,
            Err(error) => Err(self
                .resume_after_failure(&quiesce, request, freeze, prepared, error)
                .await),
        }
    }

    pub async fn pause_filesystem(
        &self,
        request: &FilesystemCheckpointRequest,
    ) -> Result<FilesystemCheckpoint, InPlaceFailure> {
        self.wait_for_rootfs_seal().await.map_err(InPlaceFailure::alive)?;
        let quiesce = self.guest_quiesce(request.flags);
        let prepared = self.quiesce_guest(&quiesce, request).await?;
        let exported = match self.pause_vm().await {
            Ok(()) => self.export_filesystem(request, prepared.frozen, false).await,
            Err(e) => Err(e),
        };
        match exported {
            Ok(checkpoint) => {
                count_fs_quiesced_pause(checkpoint.frozen);
                Ok(checkpoint)
            }
            Err(error) => {
                quiesce.rollback(prepared.method).await;
                Err(InPlaceFailure::alive(error))
            }
        }
    }

    async fn quiesce_guest(
        &self,
        quiesce: &GuestQuiesce,
        request: &FilesystemCheckpointRequest,
    ) -> Result<Preparation, InPlaceFailure> {
        let started = Instant::now();
        let prepared = quiesce.prepare().await;
        let (success, method) = match &prepared {
            Ok(Preparation { method, .. }) | Err(PreparationFailure { method, .. }) => (prepared.is_ok(), *method),
        };
        GuestSyncOutcome {
            success,
            method: method.as_str().to_owned(),
            froze: method.froze(),
            timeout: quiesce.sync_timeout(),
            team_id: request.team_id.clone(),
        }
        .record(started.elapsed());
        match prepared {
            Ok(prepared) => Ok(prepared),
            Err(failure) => {
                quiesce.rollback(failure.method).await;
                Err(InPlaceFailure::alive(failure.error))
            }
        }
    }

    async fn pause_vm(&self) -> Result<(), String> {
        let client = self.resources.vm.as_ref().ok_or("firecracker is not running")?.client();
        within_state_flip(client.pause())
            .await
            .map_err(|e| format!("failed to pause VM: {e}"))
    }

    async fn resume_vm(&self) -> Result<(), String> {
        let client = self.resources.vm.as_ref().ok_or("firecracker is not running")?.client();
        within_state_flip(client.resume()).await
    }

    async fn export_filesystem(
        &self,
        request: &FilesystemCheckpointRequest,
        frozen: bool,
        in_place: bool,
    ) -> Result<FilesystemCheckpoint, String> {
        let client = self.resources.vm.as_ref().ok_or("firecracker is not running")?.client();
        std::fs::create_dir_all(&request.dir).map_err(|e| format!("create {}: {e}", request.dir.display()))?;
        let files = SnapshotFiles::in_dir(&request.dir);
        client
            .create_snapshot(&files.snapfile)
            .await
            .map_err(|e| format!("error creating snapshot: {e}"))?;
        let started = Instant::now();
        let defer = request.defer_rootfs;
        let exported = match in_place {
            true => self.export_disk_in_place(&files.rootfs, request.build_id, defer).await,
            false => self.export_disk_with(&files.rootfs, request.build_id, defer).await,
        };
        drop(std::fs::remove_file(&files.snapfile));
        ProcessRootfsOutcome {
            fs_only: true,
            deferred: exported.as_ref().is_ok_and(|disk| disk.deferred),
            in_place,
            success: exported.is_ok(),
            team_id: request.team_id.clone(),
        }
        .record(started.elapsed());
        let disk = exported
            .inspect_err(|_| files.remove())
            .map_err(|e| export_failure(in_place, defer, e))?;
        let Exported { header, bytes } = disk.exported;
        Ok(FilesystemCheckpoint {
            build: Build::filesystem_only(request.build_id, header),
            rootfs_diff: files.rootfs,
            rootfs_bytes: bytes,
            frozen,
            vm_vcpus: self.resolved_vm_vcpus,
            rootfs_sealed: disk.sealed,
        })
    }

    async fn resume_after_snapshot(
        &self,
        quiesce: &GuestQuiesce,
        request: &FilesystemCheckpointRequest,
        freeze: Freeze,
        checkpoint: FilesystemCheckpoint,
    ) -> Result<FilesystemCheckpoint, InPlaceFailure> {
        self.resume_vm().await.map_err(|e| {
            InPlaceFailure::lost(
                LostSandbox::ResumeFailed,
                format!("resume in place failed, sandbox torn down: {SANDBOX_LOST}\n{e}"),
            )
        })?;
        record_freeze(request, &freeze, true);
        if checkpoint.frozen {
            self.thaw_or_lose(quiesce, request).await?;
        }
        self.reinit_envd();
        count_fs_quiesced_pause(checkpoint.frozen);
        Ok(checkpoint)
    }

    async fn resume_after_failure(
        &self,
        quiesce: &GuestQuiesce,
        request: &FilesystemCheckpointRequest,
        freeze: Freeze,
        prepared: Preparation,
        error: String,
    ) -> InPlaceFailure {
        if let Err(e) = self.resume_vm().await {
            return InPlaceFailure::lost(
                LostSandbox::ResumeFailed,
                format!("resume in place failed during pause cleanup, sandbox torn down: {SANDBOX_LOST}\n{e}"),
            );
        }
        if freeze.paused {
            record_freeze(request, &freeze, false);
        }
        if prepared.frozen
            && let Err(lost) = self.thaw_or_lose(quiesce, request).await
        {
            return lost;
        }
        if freeze.paused {
            self.reinit_envd();
        }
        InPlaceFailure::alive(error)
    }

    async fn thaw_or_lose(
        &self,
        quiesce: &GuestQuiesce,
        request: &FilesystemCheckpointRequest,
    ) -> Result<(), InPlaceFailure> {
        quiesce
            .thaw(|attempt, success, took| record_fsthaw(attempt, success, took, &request.team_id))
            .await
            .map_err(|e| {
                InPlaceFailure::lost(
                    LostSandbox::ThawFailed,
                    format!("rootfs thaw failed after in-place resume, sandbox torn down: {SANDBOX_LOST}\n{THAW_FAILED}\n{e}"),
                )
            })
    }

    fn reinit_envd(&self) {
        let address = self.envd_address();
        let settings = self.config.envd.with_cpu_count(self.guest_cpu_count());
        let budget = self.config.envd_timeout;
        let cancel = CancellationToken::new();
        *self.reinit.lock() = Some(cancel.clone().drop_guard());
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                reinit = envd::init(address, &settings, tokio::time::Instant::now() + budget) => {
                    if let Err(e) = reinit {
                        tracing::warn!(error = %e, "envd re-init after in-place resume failed (guest clock may lag)");
                    }
                }
            }
        });
    }
}

fn record_freeze(request: &FilesystemCheckpointRequest, freeze: &Freeze, success: bool) {
    GuestFreezeOutcome {
        deferred: false,
        fs_only: true,
        balloon_mode: request.balloon_mode.clone(),
        success,
        team_id: request.team_id.clone(),
    }
    .record(freeze.started.elapsed());
}

fn record_freeze_for(team_id: &str, balloon_mode: &str, deferred: bool, freeze: &Freeze, success: bool) {
    GuestFreezeOutcome {
        deferred,
        fs_only: false,
        balloon_mode: balloon_mode.to_owned(),
        success,
        team_id: team_id.to_owned(),
    }
    .record(freeze.started.elapsed());
}

async fn within_state_flip(call: impl std::future::Future<Output = Result<(), String>>) -> Result<(), String> {
    tokio::time::timeout(STATE_FLIP_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()))
}

fn export_failure(in_place: bool, deferred: bool, error: String) -> String {
    match (in_place, deferred) {
        (true, true) => format!("in-place rootfs export setup failed: {error}"),
        (true, false) => format!("synchronous in-place rootfs export failed: {error}"),
        (false, true) => format!("deferred rootfs export setup failed: {error}"),
        (false, false) => format!("synchronous rootfs export failed: {error}"),
    }
}
