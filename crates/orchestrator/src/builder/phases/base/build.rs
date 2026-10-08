use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc::unbounded_channel;
use uuid::Uuid;

use super::{BasePhase, BaseSource};
use crate::builder::ext4::{BLOCK_SIZE, Ext4};
use crate::builder::logs::ScopedLog;
use crate::builder::oci::pull::{ImageConfig, parse_envs};
use crate::builder::phase::{BuildError, Phase};
use crate::builder::provision::wait_for_provisioning;
use crate::builder::rootfs::{MB, RootfsRequest, create_ext4_filesystem};
use crate::builder::template_files::{BUSYBOX_INIT_PATH, InjectedSources, PROVISION_RESULT_PATH, provision_script};
use crate::builder::uploads::LayerUpload;
use crate::envd::process::{CommandContext, run_command};
use crate::envd::version::is_at_least;
use crate::go_exec::GoCommand;
use crate::network::pool::SlotLease;
use crate::network::slot::HostSlots;
use crate::proto::LogLevel;
use crate::sandbox::Sandbox;
use crate::sandbox::config::SandboxBuilder;
use crate::sandbox::snapshot::LocalSnapshot;
use crate::server::node::BuildNetwork;
use crate::storage::backend::ObjectMetadata;
use crate::template::metadata::TemplateMetadata;

const BUILD_DIR_MODE: u32 = 0o777;
const ROOTFS_FILE: &str = "rootfs.filesystem.build";
const IO_ENGINE: &str = "Sync";
const SYSTEMD_INIT: &str = "/sbin/init";
const RESERVED_DISK_SPACE_MB: i64 = 256;
const ENVD_WAIT: Duration = Duration::from_secs(60);
const MIN_ENVD_FOR_KVM_CLOCK: &str = "0.2.11";
const SYNC_COMMAND: &str = "/usr/bin/busybox sync";
const BUILD_ORIGIN: &str = "template_build_cache";
const SANDBOX_ID_LEN: usize = 20;
const ROOT_USER: &str = "root";

pub struct BaseBuild<'a> {
    pub phase: &'a BasePhase,
    pub log: &'a ScopedLog<'a>,
}

struct SlotHold {
    network: BuildNetwork,
    lease: SlotLease<HostSlots>,
}

impl SlotHold {
    fn release(self) {
        drop(self.network);
        drop(self.lease);
    }
}

struct HeldSlot {
    builder: SandboxBuilder,
    slot: SlotHold,
}

struct BuildDir {
    path: PathBuf,
}

impl Drop for BuildDir {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.path);
        let remove = move || {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::error!(error = %e, "Error while removing template build directory");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

impl BaseBuild<'_> {
    pub async fn run(&self, mut metadata: TemplateMetadata, hash: &str) -> Result<TemplateMetadata, BuildError> {
        let dir = self.build_dir(&metadata.template.build_id)?;
        let rootfs = dir.path.join(ROOTFS_FILE);
        let config = self
            .construct_rootfs(&rootfs)
            .await
            .map_err(|e| internal_context("error building environment", e))?;
        metadata.context.env_vars = parse_envs(config.config.env.as_deref().unwrap_or_default());
        self.log.push(LogLevel::Info, "Provisioning sandbox template");
        self.provision(&rootfs)
            .await
            .map_err(|e| internal_context("error provisioning sandbox", e))?;
        let ext4 = Ext4::new(&rootfs);
        self.check_provisioned(&ext4).await?;
        self.enlarge_after_provisioning(&ext4)
            .await
            .map_err(|e| BuildError::Internal(format!("error enlarging disk after provisioning: {e}")))?;
        self.log.push(LogLevel::Debug, "Creating base sandbox template layer");
        self.build_layer(&ext4, metadata, hash)
            .await
            .map_err(|e| internal_context("error building base layer", e))
    }

    fn build_dir(&self, build_id: &str) -> Result<BuildDir, BuildError> {
        let path = self.phase.env.config.templates_dir.join(build_id);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(BUILD_DIR_MODE)
            .create(&path)
            .map(|()| BuildDir { path })
            .map_err(|e| BuildError::Internal(format!("error creating template build directory: {e}")))
    }

    async fn construct_rootfs(&self, rootfs: &Path) -> Result<ImageConfig, BuildError> {
        let BaseSource::Image { image, registry } = &self.phase.request.source else {
            return Err(BuildError::Internal("build has no base image or template".to_owned()));
        };
        let script = provision_script(&self.phase.env.config.provider)
            .map_err(|e| BuildError::Internal(format!("error getting provision script: {e}")))?;
        let injected = self.injected_sources(script);
        create_ext4_filesystem(
            self.log,
            &self.phase.env.puller,
            RootfsRequest {
                meta: &self.phase.meta(),
                from_image: image,
                registry: registry.as_ref(),
                disk_size_mb: self.phase.request.disk_size_mb,
                injected,
                path: rootfs,
            },
        )
        .await
        .map_err(|e| internal_context("error creating ext4 filesystem", e))
    }

    fn injected_sources(&self, provision_script: String) -> InjectedSources {
        let config = &self.phase.env.config;
        InjectedSources {
            envd: config.envd.clone(),
            busybox: config.busybox.clone(),
            provision_script,
            memory_mb: self.phase.request.memory_mb,
            envd_memory_protection: false,
        }
    }

    async fn provision(&self, rootfs: &Path) -> Result<(), BuildError> {
        let (console, lines) = unbounded_channel();
        let held = self.build_slot("error creating sandbox").await?;
        let sandbox = held
            .builder
            .rootfs_file(rootfs)
            .init_path(BUSYBOX_INIT_PATH)
            .kernel_logs(true)
            .kvm_clock(true)
            .skip_envd()
            .console(console)
            .cold_boot()
            .await
            .map_err(|e| BuildError::Internal(format!("error creating sandbox: {e}")))?;
        if let Err(e) = wait_for_provisioning(self.log, lines).await {
            let _ = sandbox.stop().await;
            return Err(BuildError::user(
                &self.phase.meta(),
                format!("error waiting for provisioning sandbox: {e}"),
            ));
        }
        self.log
            .push(LogLevel::Info, "Provisioning was successful, cleaning up");
        sandbox
            .shut_down()
            .await
            .map_err(|e| BuildError::Internal(format!("error shutting down provisioned sandbox: {e}")))?;
        held.slot.release();
        self.remove_result(rootfs).await
    }

    async fn remove_result(&self, rootfs: &Path) -> Result<(), BuildError> {
        Ext4::new(rootfs)
            .remove_file(PROVISION_RESULT_PATH)
            .await
            .map_err(|e| BuildError::Internal(format!("result file cleanup failed: {e}")))?;
        self.log.push(LogLevel::Info, "Sandbox template provisioned");
        Ok(())
    }

    async fn build_slot(&self, context: &str) -> Result<HeldSlot, BuildError> {
        let node = &self.phase.env.node;
        let lease = node
            .network
            .lease(None)
            .await
            .map_err(|e| BuildError::Internal(format!("{context}: {e}")))?;
        let slot = lease
            .slot()
            .ok_or_else(|| BuildError::Internal(format!("{context}: no network slot")))?;
        let (sandbox_id, builder) = self.sandbox_builder();
        Ok(HeldSlot {
            builder: builder.pooled_slot(slot),
            slot: SlotHold {
                network: node.assign_build_network(slot.host_ip(), &sandbox_id),
                lease,
            },
        })
    }

    fn sandbox_builder(&self) -> (String, SandboxBuilder) {
        let id: String = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(SANDBOX_ID_LEN)
            .collect();
        let sandbox_id = format!("b{id}");
        let builder = self
            .phase
            .env
            .node
            .build_sandbox(&sandbox_id, &self.phase.request.template_id)
            .vcpus(self.phase.request.vcpus as u32)
            .memory_mib(self.phase.request.memory_mb as u32)
            .io_engine(IO_ENGINE)
            .without_mmds();
        (sandbox_id, builder)
    }

    async fn check_provisioned(&self, ext4: &Ext4) -> Result<(), BuildError> {
        match ext4.check_integrity(true).await {
            Ok(result) => {
                tracing::debug!(result = %result, "provisioned filesystem ext4 integrity");
                Ok(())
            }
            Err(e) => {
                tracing::error!(error = %e, "provisioned filesystem ext4 integrity");
                Err(BuildError::Internal(format!(
                    "error checking provisioned filesystem integrity: {e}"
                )))
            }
        }
    }

    async fn enlarge_after_provisioning(&self, ext4: &Ext4) -> Result<(), String> {
        let free = ext4
            .free_space(BLOCK_SIZE)
            .await
            .map_err(|e| format!("error getting free space: {e}"))?;
        let size_diff = self.phase.request.disk_size_mb * MB - free;
        tracing::debug!(
            size_add = size_diff,
            size_free = free,
            size_target = self.phase.request.disk_size_mb * MB,
            "adding provision size diff to rootfs"
        );
        if size_diff <= 0 {
            tracing::debug!("no need to enlarge rootfs, skipping");
            return Ok(());
        }
        let final_size = match ext4.enlarge(size_diff).await {
            Ok(size) => size,
            Err(e) => {
                let (output, error) = tune2fs_listing(&ext4.file).await;
                tracing::error!(error = ?error, "{output}");
                return Err(format!("error enlarging rootfs: {e}"));
            }
        };
        self.final_integrity(ext4).await?;
        let size = std::fs::metadata(&ext4.file)
            .map_err(|e| format!("error getting rootfs file info: {e}"))?
            .len() as i64;
        match final_size == size {
            true => Ok(()),
            false => Err(format!("size mismatch: expected {final_size}, got {size}")),
        }
    }

    async fn final_integrity(&self, ext4: &Ext4) -> Result<(), String> {
        let first = match ext4.check_integrity(false).await {
            Ok(result) => {
                tracing::debug!(result = %result, "final enlarge filesystem ext4 integrity");
                return Ok(());
            }
            Err(e) => e,
        };
        tracing::error!(result = %first, error = %first, "final enlarge filesystem ext4 integrity");
        let retried = ext4.check_integrity(true).await;
        tracing::error!(result = ?retried, "final enlarge filesystem ext4 integrity - retry with fix");
        retried
            .map(drop)
            .map_err(|e| format!("error checking final enlarge filesystem integrity: {e}"))
    }

    async fn build_layer(
        &self,
        ext4: &Ext4,
        metadata: TemplateMetadata,
        hash: &str,
    ) -> Result<TemplateMetadata, BuildError> {
        let kvm_clock = is_at_least(&self.phase.request.envd_version, MIN_ENVD_FOR_KVM_CLOCK)
            .map_err(|e| BuildError::Internal(format!("error comparing envd version: {e}")))?;
        ext4.set_reserved_blocks(RESERVED_DISK_SPACE_MB, BLOCK_SIZE)
            .await
            .map_err(|e| BuildError::Internal(format!("create sandbox: pre-boot hook failed: {e}")))?;
        let base_build = Uuid::parse_str(&metadata.template.build_id)
            .map_err(|e| BuildError::Internal(format!("failed to parse build id: {e}")))?;
        let held = self.build_slot("create sandbox").await?;
        let mut sandbox = held
            .builder
            .memory_base(base_build)
            .rootfs_file(&ext4.file)
            .init_path(SYSTEMD_INIT)
            .kernel_logs(self.phase.env.development)
            .kvm_clock(kvm_clock)
            .envd_timeout(ENVD_WAIT)
            .skip_envd()
            .cold_boot()
            .await
            .map_err(|e| BuildError::Internal(format!("create sandbox: {e}")))?;
        if let Err(e) = sandbox.wait_for_envd().await {
            let _ = sandbox.stop().await;
            return Err(BuildError::Internal(format!("wait for envd: {e}")));
        }
        if let Err(e) = self.sync(&sandbox).await {
            let _ = sandbox.stop().await;
            return Err(BuildError::Internal(format!("error running sync command: {e}")));
        }
        let build_id = Uuid::parse_str(&metadata.template.build_id)
            .map_err(|e| BuildError::Internal(format!("pause and upload: error processing vm: {e}")))?;
        let dir = self.phase.env.node.layer_dir(build_id);
        let local = sandbox
            .pause_into(&dir, build_id)
            .await
            .map_err(|e| BuildError::Internal(format!("pause and upload: error processing vm: {e}")))?;
        held.slot.release();
        self.upload(local, dir, metadata, hash).await
    }

    async fn sync(&self, sandbox: &Sandbox) -> Result<(), String> {
        run_command(
            sandbox.envd_address(),
            SYNC_COMMAND,
            &CommandContext {
                user: ROOT_USER.to_owned(),
                ..CommandContext::default()
            },
            |_, _| {},
        )
        .await
    }

    async fn upload(
        &self,
        local: LocalSnapshot,
        dir: PathBuf,
        metadata: TemplateMetadata,
        hash: &str,
    ) -> Result<TemplateMetadata, BuildError> {
        let node = &self.phase.env.node;
        let build_id = &metadata.template.build_id;
        self.log
            .push(LogLevel::Debug, format!("Adding layer to cache: {build_id}"));
        node.cache_layer(&local, metadata.clone(), dir).map_err(|e| {
            BuildError::Internal(format!(
                "pause and upload: error adding snapshot to template cache: {e}"
            ))
        })?;
        self.log.push(LogLevel::Debug, format!("Uploading layer: {build_id}"));
        let object_metadata = ObjectMetadata::default()
            .with("team_id", &self.phase.request.team_id)
            .with("template_id", &self.phase.request.template_id)
            .with("build_origin", BUILD_ORIGIN);
        self.phase.uploads.start(
            node.spawn_upload(local, object_metadata),
            LayerUpload {
                index: self.phase.index.clone(),
                hash: hash.to_owned(),
                build_id: build_id.clone(),
                fields: self.log.fields.clone(),
            },
        );
        Ok(metadata)
    }
}

async fn tune2fs_listing(file: &Path) -> (String, Option<String>) {
    match GoCommand::new("tune2fs")
        .args(["-l".as_ref(), file.as_os_str()])
        .run()
        .await
    {
        Ok(output) => (String::from_utf8_lossy(&output).into_owned(), None),
        Err(failure) => (
            String::from_utf8_lossy(&failure.output).into_owned(),
            Some(failure.message),
        ),
    }
}

fn internal_context(context: &str, error: BuildError) -> BuildError {
    match error {
        BuildError::Internal(e) => BuildError::Internal(format!("{context}: {e}")),
        user => user,
    }
}
