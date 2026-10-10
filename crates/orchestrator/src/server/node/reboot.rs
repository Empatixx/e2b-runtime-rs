mod recovery;

use std::time::{Duration, SystemTime};

use tracing::field::Empty;
use tracing::{Instrument, info_span};

use super::Node;
use crate::builder::layer::{DEFAULT_IO_ENGINE, ENVD_WAIT, MIN_ENVD_FOR_KVM_CLOCK, SYSTEMD_INIT};
use crate::envd::EnvdInit;
use crate::envd::version::is_at_least;
use crate::featureflags::{FlagContext, PREBOOT_FS_RECOVERY, REBOOT_CPU_TEMPLATE_OVERRIDE};
use crate::firecracker::kernel_args::{render, validate_cmdline_args};
use crate::go_time::rfc3339;
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;
use crate::sandbox::config::{PreBoot, START_REBOOT, chain_pre_boot};
use crate::sandbox::reboot::{
    RebootCpuTemplate, check_reboot_cpu_template, reboot_allowed, reboot_cpu_template, reboot_max_vcpus,
};
use crate::sandbox::rootfs::recovery::RecoveryRecord;
use crate::server::registry::Lifecycle;
use crate::storage::build::Build;
use crate::template::metadata::{CpuTemplate, TemplateMetadata, applied_digest};

const DEFAULT_USER: &str = "user";

pub(super) struct RebootRequest<'a> {
    pub config: &'a SandboxConfig,
    pub build: &'a Build,
    pub metadata: &'a TemplateMetadata,
    pub slot: crate::network::slot::Slot,
    pub init: EnvdInit,
    pub lifecycle: &'a Lifecycle,
    pub end: SystemTime,
    pub requested: bool,
    pub fs_recovery: &'a RecoveryRecord,
}

pub(super) struct Rebooted {
    pub sandbox: Sandbox,
    pub cpu_template: Option<CpuTemplate>,
}

impl Node {
    pub(super) async fn reboot(&self, request: RebootRequest<'_>) -> Result<Rebooted, String> {
        let span = info_span!(
            "reboot sandbox",
            "sandbox.build_cpu_template" = Empty,
            "sandbox.cpu_template_overridden" = Empty,
            "sandbox.cpu_template_override_invalid" = Empty,
            "sandbox.cmdline_args" = Empty,
            "sandbox.cpu_template" = Empty,
            "sandbox.filesystem_boot_requested" = request.requested,
        );
        self.boot_from_rootfs(request, &span).instrument(span.clone()).await
    }

    async fn boot_from_rootfs(&self, request: RebootRequest<'_>, span: &tracing::Span) -> Result<Rebooted, String> {
        let RebootRequest {
            config,
            build,
            metadata,
            slot,
            mut init,
            lifecycle,
            end,
            requested,
            fs_recovery,
        } = request;
        if !reboot_allowed(metadata, requested) {
            return Err(format!(
                "refusing to reboot build {}: not a filesystem-only snapshot and the request did not demand a filesystem boot",
                build.build_id
            ));
        }
        let max_vcpus = reboot_max_vcpus(config.max_vcpus, metadata.vcpu_count);
        let chosen = self.resolve_cpu_template(config, metadata, span);
        check_reboot_cpu_template(chosen.template.as_ref(), &config.firecracker_version)
            .map_err(|e| format!("reboot build {}: {e}", build.build_id))?;
        init.default_user.get_or_insert_with(|| default_user_of(metadata));
        init.default_workdir = init.default_workdir.take().or_else(|| metadata.context.workdir.clone());
        let kvm_clock = is_at_least(&config.envd_version, MIN_ENVD_FOR_KVM_CLOCK)
            .map_err(|e| format!("compare envd version: {e}"))?;
        end.duration_since(SystemTime::now())
            .ok()
            .filter(|remaining| *remaining > Duration::ZERO)
            .ok_or_else(|| format!("sandbox end time {} is in the past", rfc3339(end)))?;
        span.record("sandbox.cmdline_args", applied_cmdline(metadata).as_str());
        span.record(
            "sandbox.cpu_template",
            applied_digest(chosen.template.as_ref()).as_str(),
        );
        let pre_boot = chain_pre_boot([self
            .fs_recover_pre_boot(config, metadata.fs_quiesced, requested, fs_recovery)
            .await]);
        let work_dir = self.work_dir_of(lifecycle);
        std::fs::create_dir_all(&work_dir).map_err(|e| format!("create {}: {e}", work_dir.display()))?;
        let overlay = self.layer_overlay(build, &work_dir)?;
        let sandbox = Sandbox::builder()
            .id(&config.sandbox_id)
            .template_id(&config.template_id)
            .rootfs(overlay)
            .memory_base(build.build_id)
            .pooled_slot(slot)
            .firecracker(self.binaries.firecracker_for(&config.firecracker_version))
            .kernel(self.binaries.kernel_for(&config.kernel_version))
            .work_dir(work_dir)
            .init_path(SYSTEMD_INIT)
            .kvm_clock(kvm_clock)
            .io_engine(DEFAULT_IO_ENGINE)
            .vcpus(config.vcpu.max(0) as u32)
            .max_vcpus(max_vcpus.max(0) as u32)
            .memory_mib(config.ram_mb.max(0) as u32)
            .huge_pages(config.huge_pages)
            .envd_timeout(ENVD_WAIT)
            .hyperloop_ip(&self.orchestrator_ip)
            .envd(self.with_init_request_timeout(init))
            .envd_version(&config.envd_version)
            .team_id(&config.team_id)
            .start_type(START_REBOOT)
            .cmdline_args(metadata.cmdline_args.clone())
            .maybe_cpu_template(chosen.template.clone())
            .maybe_pre_boot(pre_boot)
            .cold_boot()
            .await
            .map_err(|e| format!("create sandbox from rootfs: {e}"))?;
        Ok(Rebooted {
            sandbox,
            cpu_template: chosen.template,
        })
    }

    async fn fs_recover_pre_boot(
        &self,
        config: &SandboxConfig,
        fs_quiesced: bool,
        requested: bool,
        record: &RecoveryRecord,
    ) -> Option<PreBoot> {
        let contexts = [
            FlagContext::sandbox(&config.sandbox_id),
            FlagContext::template(&config.template_id),
        ];
        let enabled = self.feature_flags.get(&PREBOOT_FS_RECOVERY, &contexts);
        let span = info_span!(
            "resolve fs recovery",
            "sandbox.fs_quiesced" = fs_quiesced,
            "fs_recovery.enabled" = enabled
        );
        async move {
            if !enabled {
                return None;
            }
            recovery::probe_journal_only().await;
            recovery::recovery_step(fs_quiesced, requested, record)
        }
        .instrument(span)
        .await
    }

    fn resolve_cpu_template(
        &self,
        config: &SandboxConfig,
        metadata: &TemplateMetadata,
        span: &tracing::Span,
    ) -> RebootCpuTemplate {
        let contexts = [
            FlagContext::sandbox(&config.sandbox_id),
            FlagContext::template(&config.template_id),
            FlagContext::team(&config.team_id),
        ];
        let override_value = self.feature_flags.get(&REBOOT_CPU_TEMPLATE_OVERRIDE, &contexts);
        let chosen = reboot_cpu_template(metadata, &override_value);
        if let Some(rejected) = &chosen.rejected {
            tracing::error!(
                flag = REBOOT_CPU_TEMPLATE_OVERRIDE.key,
                error = %rejected,
                sandbox.id = %config.sandbox_id,
                "ignoring invalid reboot CPU template override"
            );
        }
        if chosen.overridden {
            tracing::info!(
                build_cpu_template = %applied_digest(metadata.build_cpu_template.as_ref()),
                cpu_template = %applied_digest(chosen.template.as_ref()),
                "reboot CPU template overridden"
            );
        }
        span.record(
            "sandbox.build_cpu_template",
            applied_digest(metadata.build_cpu_template.as_ref()).as_str(),
        );
        span.record("sandbox.cpu_template_overridden", chosen.overridden);
        span.record("sandbox.cpu_template_override_invalid", chosen.rejected.is_some());
        chosen
    }
}

fn default_user_of(metadata: &TemplateMetadata) -> String {
    metadata
        .context
        .user
        .clone()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| DEFAULT_USER.to_owned())
}

fn applied_cmdline(metadata: &TemplateMetadata) -> String {
    validate_cmdline_args(&metadata.cmdline_args).map_or_else(|_| String::new(), |()| render(&metadata.cmdline_args))
}
