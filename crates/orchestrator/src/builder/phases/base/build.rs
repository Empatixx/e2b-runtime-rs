use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc::unbounded_channel;
use uuid::Uuid;

use super::{BasePhase, BaseSource};
use crate::builder::ext4::{BLOCK_SIZE, Ext4};
use crate::builder::layer::{
    BUILD_CACHE_ORIGIN, DEFAULT_IO_ENGINE, LayerExecutor, MIN_ENVD_FOR_KVM_CLOCK, SandboxShape, new_sandbox_id,
    sync_changes_to_disk,
};
use crate::builder::logs::ScopedLog;
use crate::builder::oci::pull::{ImageConfig, parse_envs};
use crate::builder::phase::{BuildError, Phase};
use crate::builder::provision::wait_for_provisioning;
use crate::builder::rootfs::{MB, RootfsRequest, create_ext4_filesystem};
use crate::builder::template_files::{BUSYBOX_INIT_PATH, InjectedSources, PROVISION_RESULT_PATH, provision_script};
use crate::envd::version::is_at_least;
use crate::go_exec::GoCommand;
use crate::proto::LogLevel;
use crate::sandbox::config::SandboxBuilder;
use crate::template::metadata::TemplateMetadata;

const BUILD_DIR_MODE: u32 = 0o777;
const ROOTFS_FILE: &str = "rootfs.filesystem.build";

pub struct BaseBuild<'a> {
    pub phase: &'a BasePhase,
    pub log: &'a ScopedLog<'a>,
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
        metadata.context.env_vars = Some(parse_envs(config.config.env.as_deref().unwrap_or_default()));
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
                max_rootfs_size_mb: self.phase.settings.flags.base_rootfs_size_limit_mb(),
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
            envd_memory_protection: self.phase.settings.envd_memory_protection,
        }
    }

    async fn provision(&self, rootfs: &Path) -> Result<(), BuildError> {
        let (console, lines) = unbounded_channel();
        let sandbox_id = new_sandbox_id();
        let slot = self
            .executor()
            .lease_slot(&sandbox_id, "error creating sandbox")
            .await?;
        let sandbox = slot
            .attach(self.sandbox_builder(&sandbox_id))
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

    fn executor(&self) -> LayerExecutor<'_> {
        LayerExecutor {
            env: &self.phase.env,
            settings: &self.phase.settings,
            index: &self.phase.index,
            uploads: &self.phase.uploads,
            log: self.log,
            template_id: &self.phase.request.template_id,
            team_id: &self.phase.request.team_id,
        }
    }

    fn shape(&self) -> SandboxShape<'_> {
        SandboxShape {
            vcpus: self.phase.request.vcpus,
            memory_mb: self.phase.request.memory_mb,
            envd_version: &self.phase.request.envd_version,
            io_engine: DEFAULT_IO_ENGINE,
        }
    }

    fn sandbox_builder(&self, sandbox_id: &str) -> SandboxBuilder {
        self.phase
            .env
            .node
            .build_sandbox(sandbox_id, &self.phase.request.template_id)
            .vcpus(self.phase.request.vcpus as u32)
            .memory_mib(self.phase.request.memory_mb as u32)
            .io_engine(DEFAULT_IO_ENGINE)
            .without_mmds()
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
        ext4.set_reserved_blocks(self.phase.settings.flags.reserved_disk_space_mb(), BLOCK_SIZE)
            .await
            .map_err(|e| BuildError::Internal(format!("create sandbox: pre-boot hook failed: {e}")))?;
        let base_build = Uuid::parse_str(&metadata.template.build_id)
            .map_err(|e| BuildError::Internal(format!("failed to parse build id: {e}")))?;
        let executor = self.executor();
        let sandbox_id = new_sandbox_id();
        let slot = executor.lease_slot(&sandbox_id, "create sandbox").await?;
        let builder = self
            .phase
            .env
            .node
            .build_sandbox(&sandbox_id, &self.phase.request.template_id);
        let mut sandbox = executor
            .boot_options(slot.attach(builder), &self.shape())
            .memory_base(base_build)
            .rootfs_file(&ext4.file)
            .kvm_clock(kvm_clock)
            .cold_boot()
            .await
            .map_err(|e| BuildError::Internal(format!("create sandbox: {e}")))?;
        if let Err(e) = sandbox.wait_for_envd().await {
            let _ = sandbox.stop().await;
            return Err(BuildError::Internal(format!("wait for envd: {e}")));
        }
        if let Err(e) = sync_changes_to_disk(sandbox.envd_address()).await {
            let _ = sandbox.stop().await;
            return Err(BuildError::Internal(format!("error running sync command: {e}")));
        }
        executor
            .pause_and_upload(sandbox, hash, metadata, BUILD_CACHE_ORIGIN)
            .await
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
