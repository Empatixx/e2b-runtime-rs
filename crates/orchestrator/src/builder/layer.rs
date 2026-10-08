mod filesystem;

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use uuid::Uuid;

use super::ext4::{BLOCK_SIZE, Ext4};
use super::index::LayerIndex;
use super::logs::ScopedLog;
use super::phase::BuildError;
use super::phases::BuildEnv;
use super::sandbox_command::{CommandLog, ROOT, run_logged, run_quiet};
use super::uploads::{LayerUpload, LayerUploads};
use crate::envd::EnvdDefaults;
use crate::envd::files::copy_file;
use crate::envd::process::CommandContext;
use crate::envd::version::is_at_least;
use crate::go_exec::GoCommand;
use crate::network::pool::SlotLease;
use crate::network::slot::{HostSlots, Slot};
use crate::proto::LogLevel;
use crate::sandbox::Sandbox;
use crate::sandbox::config::{PreBoot, SandboxBuilder};
use crate::sandbox::snapshot::LocalSnapshot;
use crate::server::node::BuildNetwork;
use crate::server::node::layers::{LayerSandbox, LayerSource};
use crate::storage::backend::ObjectMetadata;
use crate::template::metadata::{TemplateMetadata, TemplateVersions};

pub const IO_ENGINE: &str = "Sync";
pub const SYSTEMD_INIT: &str = "/sbin/init";
pub const RESERVED_DISK_SPACE_MB: i64 = 256;
pub const ENVD_WAIT: Duration = Duration::from_secs(60);
pub const MIN_ENVD_FOR_KVM_CLOCK: &str = "0.2.11";
const SYNC_COMMAND: &str = "/usr/bin/busybox sync";
pub const BUILD_CACHE_ORIGIN: &str = "template_build_cache";
pub const BUILD_ORIGIN: &str = "template_build";
const SANDBOX_ID_LEN: usize = 20;
const TMP_ENVD_PATH: &str = "/tmp/envd_updated";
const GUEST_ENVD_PATH: &str = "/usr/bin/envd";
const REPLACE_LOG_ID: &str = "update-envd-replace";
const RESTART_ENVD: &str = "if command -v systemctl >/dev/null 2>&1; then
    systemctl restart envd
else
    kill -TERM \"$(pidof envd)\"
fi";

pub async fn envd_version(envd: &Path) -> Result<String, String> {
    GoCommand::new(envd)
        .args(["-version"])
        .run()
        .await
        .map(|output| String::from_utf8_lossy(&output).trim().to_owned())
        .map_err(|e| format!("error while getting envd version: {}", e.message))
}

pub async fn update_envd(log: &ScopedLog<'_>, sandbox: &mut Sandbox, host_envd: &Path) -> Result<(), String> {
    let version = envd_version(host_envd)
        .await
        .map_err(|e| format!("error getting envd version: {e}"))?;
    log.push(LogLevel::Debug, format!("Updating envd to version v{version}"));
    let envd = sandbox.envd_address();
    copy_file(envd, ROOT, host_envd, TMP_ENVD_PATH)
        .await
        .map_err(|e| format!("failed to copy envd binary to sandbox: {e}"))?;
    let root = CommandContext {
        user: ROOT.to_owned(),
        ..CommandContext::default()
    };
    let output = CommandLog {
        log,
        level: LogLevel::Debug,
        id: REPLACE_LOG_ID,
    };
    run_logged(envd, &replace_script(), &root, &output)
        .await
        .map_err(|e| format!("failed to replace envd binary: {e}"))?;
    let _ = run_quiet(envd, RESTART_ENVD, &root).await;
    sandbox
        .wait_for_envd()
        .await
        .map_err(|e| format!("failed to wait for envd initialization after update: {e}"))
}

fn replace_script() -> String {
    format!(
        "\n\t\t# Replace the binary and set permissions\n\t\tchmod +x {TMP_ENVD_PATH}\n\t\tmv -f {TMP_ENVD_PATH} {GUEST_ENVD_PATH}\n\t"
    )
}

pub struct LayerExecutor<'a> {
    pub env: &'a BuildEnv,
    pub index: &'a Arc<LayerIndex>,
    pub uploads: &'a LayerUploads,
    pub log: &'a ScopedLog<'a>,
    pub template_id: &'a str,
    pub team_id: &'a str,
}

pub struct SandboxShape<'a> {
    pub vcpus: i64,
    pub memory_mb: i64,
    pub envd_version: &'a str,
}

pub enum SandboxCreator {
    Resume,
    ColdBoot,
}

pub struct LayerBuild<'a> {
    pub source_build_id: &'a str,
    pub current: TemplateMetadata,
    pub hash: &'a str,
    pub update_envd: bool,
    pub creator: SandboxCreator,
    pub shape: SandboxShape<'a>,
    pub envd_defaults: EnvdDefaults,
    pub reserved_blocks: bool,
    pub build_origin: &'a str,
}

pub struct BuildSlot {
    slot: Slot,
    network: BuildNetwork,
    lease: SlotLease<HostSlots>,
}

impl BuildSlot {
    pub fn attach(self, builder: SandboxBuilder) -> SandboxBuilder {
        let BuildSlot { slot, network, lease } = self;
        builder.pooled_slot(slot).hold(network).hold(lease)
    }
}

fn snapshot_error(cause: String) -> BuildError {
    BuildError::Internal(format!("get template snapshot: get template snapshot: {cause}"))
}

async fn fail(sandbox: Sandbox, error: BuildError) -> BuildError {
    let _ = sandbox.stop().await;
    error
}

impl LayerExecutor<'_> {
    pub async fn build_layer<A, F>(&self, build: LayerBuild<'_>, action: A) -> Result<TemplateMetadata, BuildError>
    where
        A: FnOnce(SocketAddr, TemplateMetadata) -> F,
        F: Future<Output = Result<TemplateMetadata, BuildError>>,
    {
        let source = self
            .env
            .node
            .layer_source(build.source_build_id)
            .await
            .map_err(snapshot_error)?;
        let mut sandbox = self.create_sandbox(&build, &source).await?;
        if build.update_envd
            && let Err(e) = update_envd(self.log, &mut sandbox, &self.env.config.envd).await
        {
            tracing::error!(error = %e, "error updating envd");
            return Err(fail(sandbox, BuildError::Internal(format!("update envd: {e}"))).await);
        }
        let build_id = build.current.template.build_id.clone();
        let metadata = match action(sandbox.envd_address(), build.current).await {
            Ok(metadata) => metadata,
            Err(e) => {
                tracing::error!(error = ?e, "error executing action");
                return Err(fail(sandbox, e).await);
            }
        };
        let metadata = metadata.new_version(TemplateVersions {
            build_id,
            kernel_version: self.env.config.kernel_version.clone(),
            firecracker_version: self.env.config.firecracker_version.clone(),
        });
        self.pause_and_upload(sandbox, build.hash, metadata, build.build_origin)
            .await
    }

    async fn create_sandbox(&self, build: &LayerBuild<'_>, source: &LayerSource) -> Result<Sandbox, BuildError> {
        let sandbox_id = new_sandbox_id();
        let layer = self.layer_sandbox(source, &sandbox_id);
        match build.creator {
            SandboxCreator::Resume => self.resume(&layer).await,
            SandboxCreator::ColdBoot => self.cold_boot(&layer, build).await,
        }
    }

    pub async fn resume_source(&self, source: &LayerSource) -> Result<Sandbox, BuildError> {
        let sandbox_id = new_sandbox_id();
        self.resume(&self.layer_sandbox(source, &sandbox_id)).await
    }

    fn layer_sandbox<'s>(&'s self, source: &'s LayerSource, sandbox_id: &'s str) -> LayerSandbox<'s> {
        LayerSandbox {
            source,
            sandbox_id,
            template_id: self.template_id,
            firecracker_version: &self.env.config.firecracker_version,
        }
    }

    async fn resume(&self, layer: &LayerSandbox<'_>) -> Result<Sandbox, BuildError> {
        let resumed = |e: String| BuildError::Internal(format!("resume sandbox: {e}"));
        let snapfile = layer
            .source
            .snapfile
            .as_deref()
            .ok_or_else(|| resumed("the source layer has no snapfile".to_owned()))?;
        let slot = self.lease_slot(layer.sandbox_id, "resume sandbox").await?;
        slot.attach(self.env.node.resume_layer(layer).map_err(resumed)?)
            .envd_timeout(ENVD_WAIT)
            .resume(snapfile)
            .await
            .map_err(resumed)
    }

    async fn cold_boot(&self, layer: &LayerSandbox<'_>, build: &LayerBuild<'_>) -> Result<Sandbox, BuildError> {
        let created = |e: String| BuildError::Internal(format!("create sandbox: {e}"));
        let kvm_clock = is_at_least(build.shape.envd_version, MIN_ENVD_FOR_KVM_CLOCK)
            .map_err(|e| BuildError::Internal(format!("error comparing envd version: {e}")))?;
        let slot = self.lease_slot(layer.sandbox_id, "create sandbox").await?;
        let builder = self.env.node.cold_boot_layer(layer).map_err(created)?;
        let builder = self
            .boot_options(slot.attach(builder), &build.shape)
            .kvm_clock(kvm_clock)
            .envd(self.env.node.build_envd(build.envd_defaults.clone()));
        let builder = match build.reserved_blocks {
            true => builder.pre_boot(reserved_blocks()),
            false => builder,
        };
        let mut sandbox = builder.cold_boot().await.map_err(created)?;
        match sandbox.wait_for_envd().await {
            Ok(()) => Ok(sandbox),
            Err(e) => Err(fail(sandbox, BuildError::Internal(format!("wait for envd: {e}"))).await),
        }
    }

    pub fn boot_options(&self, builder: SandboxBuilder, shape: &SandboxShape<'_>) -> SandboxBuilder {
        builder
            .vcpus(shape.vcpus as u32)
            .memory_mib(shape.memory_mb as u32)
            .io_engine(IO_ENGINE)
            .without_mmds()
            .init_path(SYSTEMD_INIT)
            .kernel_logs(self.env.development)
            .envd_timeout(ENVD_WAIT)
            .skip_envd()
    }

    pub async fn lease_slot(&self, sandbox_id: &str, context: &str) -> Result<BuildSlot, BuildError> {
        let node = &self.env.node;
        let lease = node
            .network
            .lease(None)
            .await
            .map_err(|e| BuildError::Internal(format!("{context}: {e}")))?;
        let slot = lease
            .slot()
            .ok_or_else(|| BuildError::Internal(format!("{context}: no network slot")))?;
        Ok(BuildSlot {
            slot,
            network: node.assign_build_network(slot.host_ip(), sandbox_id),
            lease,
        })
    }

    pub async fn pause_and_upload(
        &self,
        sandbox: Sandbox,
        hash: &str,
        metadata: TemplateMetadata,
        build_origin: &str,
    ) -> Result<TemplateMetadata, BuildError> {
        let paused = |e: String| BuildError::Internal(format!("pause and upload: {e}"));
        let build_id =
            Uuid::parse_str(&metadata.template.build_id).map_err(|e| paused(format!("error processing vm: {e}")))?;
        let dir = self.env.node.layer_dir(build_id);
        let local = sandbox
            .pause_into(&dir, build_id)
            .await
            .map_err(|e| paused(format!("error processing vm: {e}")))?;
        self.upload(local, dir, &metadata, hash, build_origin).map_err(paused)?;
        Ok(metadata)
    }

    fn upload(
        &self,
        local: LocalSnapshot,
        dir: PathBuf,
        metadata: &TemplateMetadata,
        hash: &str,
        build_origin: &str,
    ) -> Result<(), String> {
        let node = &self.env.node;
        let build_id = &metadata.template.build_id;
        self.log
            .push(LogLevel::Debug, format!("Adding layer to cache: {build_id}"));
        node.cache_layer(&local, metadata.clone(), dir)
            .map_err(|e| format!("error adding snapshot to template cache: {e}"))?;
        self.log.push(LogLevel::Debug, format!("Uploading layer: {build_id}"));
        self.uploads.start(
            node.spawn_upload(local, self.object_metadata(build_origin)),
            LayerUpload {
                index: self.index.clone(),
                hash: hash.to_owned(),
                build_id: build_id.clone(),
                fields: self.log.fields.clone(),
            },
        );
        Ok(())
    }

    pub fn object_metadata(&self, build_origin: &str) -> ObjectMetadata {
        ObjectMetadata::default()
            .with("team_id", self.team_id)
            .with("template_id", self.template_id)
            .with("build_origin", build_origin)
    }
}

pub fn new_sandbox_id() -> String {
    let id: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(SANDBOX_ID_LEN)
        .collect();
    format!("b{id}")
}

pub async fn sync_changes_to_disk(envd: SocketAddr) -> Result<(), String> {
    let root = CommandContext {
        user: ROOT.to_owned(),
        ..CommandContext::default()
    };
    run_quiet(envd, SYNC_COMMAND, &root).await
}

pub fn reserved_blocks() -> PreBoot {
    Box::new(|device: PathBuf| {
        async move {
            Ext4::new(&device)
                .set_reserved_blocks(RESERVED_DISK_SPACE_MB, BLOCK_SIZE)
                .await
        }
        .boxed()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_error_wraps_the_cache_error_twice_like_upstream() {
        assert_eq!(
            snapshot_error("boom".to_owned()),
            BuildError::Internal("get template snapshot: get template snapshot: boom".to_owned())
        );
    }

    #[test]
    fn the_replace_script_matches_upstream_bytes() {
        assert_eq!(
            replace_script(),
            "\n\t\t# Replace the binary and set permissions\n\t\tchmod +x /tmp/envd_updated\n\t\tmv -f /tmp/envd_updated /usr/bin/envd\n\t"
        );
    }
}
