use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::steps::StepsBuild;
use crate::builder::commands::{
    CommandRun, PROCESS_CANCELLED, REQUEST_CANCELLED, START_CANCELLED, as_context, root_only,
};
use crate::builder::hash::finalize_hash;
use crate::builder::layer::{BUILD_ORIGIN, LayerBuild, SandboxCreator, SandboxShape, sync_changes_to_disk};
use crate::builder::logs::ScopedLog;
use crate::builder::phase::{BuildError, LayerResult, Phase, PhaseMeta, internal_context};
use crate::envd::EnvdDefaults;
use crate::envd::process::CommandContext;
use crate::firecracker::client::BalloonCaps;
use crate::go_time;
use crate::proto::LogLevel;
use crate::template::metadata::{Balloon, Context, CpuTemplate, Start, TemplateMetadata, TemplateVersions};

const FINALIZE: &str = "finalize";
const FINALIZE_TEXT: &str = "Finalizing template build";
const CONFIGURE_SCRIPT: &str = include_str!("../../../assets/template/finalize/configure.sh");
const PACK_CERT_BUNDLE: &str = include_str!("../../../assets/template/finalize/pack_cert_bundle.sh");
const CONFIGURATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const READY_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const READY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_READY_WAIT_SECONDS: u64 = 20;
const LONG_READY_WAIT_SECONDS: u64 = 120;
const LONG_READY_TEMPLATES: [&str; 3] = ["zegbt9dl3l2ixqem82mm", "ot5bidkk3j2so2j02uuz", "0zeou1s7agaytqitvmzc"];
const NO_WAIT_READY_COMMAND: &str = "sleep 0";
const DEFAULT_USER: &str = "user";
const CONFIG_ID: &str = "config";
const START_ID: &str = "start";
const READY_ID: &str = "ready";
const CERTS_ID: &str = "certs";

pub struct FinalizePhase {
    pub build: Arc<StepsBuild>,
    pub build_id: String,
    pub start_command: String,
    pub ready_command: String,
    pub context_defaults: bool,
}

impl FinalizePhase {
    fn final_layer(&self, source: &LayerResult, hash: String) -> LayerResult {
        let settings = &self.build.settings;
        let template = TemplateVersions {
            build_id: self.build_id.clone(),
            kernel_version: settings.kernel_version.clone(),
            firecracker_version: settings.firecracker_version.clone(),
        };
        let start = start_of(&source.metadata, &self.start_command, &self.ready_command);
        let vm = BootedVm {
            template,
            vcpus: self.build.vcpus,
            cmdline_args: settings.cmdline_args.clone(),
            cpu_template: settings.cpu_template.clone(),
            balloon: settings.balloon,
        };
        LayerResult {
            metadata: final_metadata(&source.metadata, start, vm),
            cached: false,
            hash,
        }
    }

    async fn build_final(
        &self,
        log: &ScopedLog<'_>,
        source: &LayerResult,
        current: LayerResult,
    ) -> Result<LayerResult, BuildError> {
        let io_engine = self.build.settings.flags.io_engine();
        tracing::debug!(io_engine = %io_engine, "using io engine");
        let layer = LayerBuild {
            source_build_id: &source.metadata.template.build_id,
            envd_defaults: envd_defaults(&current.metadata.context, self.context_defaults),
            current: current.metadata,
            hash: &current.hash,
            update_envd: source.cached,
            creator: SandboxCreator::ColdBoot,
            shape: SandboxShape {
                io_engine: &io_engine,
                ..self.build.shape()
            },
            reserved_blocks: source.cached,
            build_origin: BUILD_ORIGIN,
        };
        let metadata = self
            .build
            .executor(log)
            .build_layer(layer, |envd, metadata| self.post_process(log, envd, metadata))
            .await
            .map_err(|e| internal_context("error running start and ready commands in sandbox", e))?;
        Ok(LayerResult {
            metadata,
            cached: false,
            hash: current.hash,
        })
    }

    async fn post_process(
        &self,
        log: &ScopedLog<'_>,
        envd: SocketAddr,
        metadata: TemplateMetadata,
    ) -> Result<TemplateMetadata, BuildError> {
        let run = CommandRun {
            envd,
            log,
            level: LogLevel::Debug,
            prefix: CONFIG_ID,
            files: &self.build.env.build_storage,
            cache_scope: &self.build.cache_scope,
            cancel: &self.build.cancel,
        };
        self.configure(&run).await?;
        if let Some(start) = &metadata.start {
            self.start_and_wait(&run, start).await?;
        }
        self.pack_cert_bundle(&run).await?;
        sync_changes_to_disk(envd)
            .await
            .map_err(|e| BuildError::Internal(format!("error running sync command: {e}")))?;
        Ok(metadata)
    }

    async fn configure(&self, run: &CommandRun<'_>) -> Result<(), BuildError> {
        let script = configure_script(&self.build.template_id, &self.build_id);
        run.logged_until(Instant::now() + CONFIGURATION_TIMEOUT, &script, &root_only())
            .await
            .map_err(|e| {
                self.user_error(format!(
                    "configuration script failed: error running configuration script: {e}"
                ))
            })
    }

    async fn start_and_wait(&self, run: &CommandRun<'_>, start: &Start) -> Result<(), BuildError> {
        let commands = self.build.cancel.child_token();
        let run = CommandRun {
            cancel: &commands,
            ..*run
        };
        let context = as_context(&start.context);
        let (started, confirmed) = oneshot::channel();
        if !start.start_command.is_empty() {
            run.log.push(
                LogLevel::Info,
                format!("Running start command: {}", start.start_command),
            );
        }
        let ready = async {
            let waited = match self.wait_until_ready(&run, &commands, start, &context).await {
                Ok(()) => self.wait_for_start(confirmed).await,
                Err(e) => Err(e),
            };
            commands.cancel();
            waited
        };
        let (start_result, ready_result) = tokio::join!(run_start(&run, &commands, start, &context, started), ready);
        ready_result?;
        start_result.map_err(|e| self.user_error(format!("start command failed: {e}")))
    }

    async fn wait_until_ready(
        &self,
        run: &CommandRun<'_>,
        commands: &CancellationToken,
        start: &Start,
        context: &CommandContext,
    ) -> Result<(), BuildError> {
        let command = ready_command(&self.build.template_id, start);
        run.log
            .push(LogLevel::Info, format!("Waiting for template to be ready: {command}"));
        let started = Instant::now();
        let deadline = started + READY_TIMEOUT;
        let ready = CommandRun {
            prefix: READY_ID,
            ..*run
        };
        loop {
            match ready.logged_until(deadline, &command, context).await {
                Ok(()) => return template_ready(run),
                Err(e) => run.log.push(LogLevel::Debug, format!("Template is not ready: {e}")),
            }
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(deadline) => return Err(self.ready_timed_out(started)),
                () = commands.cancelled() => return template_ready(run),
                () = tokio::time::sleep(READY_RETRY_INTERVAL) => {}
            }
        }
    }

    fn ready_timed_out(&self, started: Instant) -> BuildError {
        let took = go_time::duration(started.elapsed());
        self.user_error(format!("ready command failed: ready command timed out after {took}"))
    }

    async fn wait_for_start(&self, confirmed: oneshot::Receiver<()>) -> Result<(), BuildError> {
        tokio::select! {
            biased;
            () = self.build.cancel.cancelled() => {
                Err(self.user_error(format!("waiting for start command failed: {REQUEST_CANCELLED}")))
            }
            _ = confirmed => Ok(()),
        }
    }

    async fn pack_cert_bundle(&self, run: &CommandRun<'_>) -> Result<(), BuildError> {
        CommandRun {
            prefix: CERTS_ID,
            ..*run
        }
        .logged(PACK_CERT_BUNDLE, &root_only())
        .await
        .map_err(|e| self.user_error(format!("error packing CA cert bundle: {e}")))
    }

    fn user_error(&self, message: String) -> BuildError {
        BuildError::user(&self.meta(), message)
    }
}

async fn run_start(
    run: &CommandRun<'_>,
    commands: &CancellationToken,
    start: &Start,
    context: &CommandContext,
    started: oneshot::Sender<()>,
) -> Result<(), String> {
    if start.start_command.is_empty() {
        return Ok(());
    }
    let run = CommandRun {
        level: LogLevel::Info,
        prefix: START_ID,
        ..*run
    };
    match run.logged_confirmed(&start.start_command, context, started).await {
        Err(e) if e != PROCESS_CANCELLED && e != START_CANCELLED => {
            commands.cancel();
            Err(format!("error running start command: {e}"))
        }
        _ => Ok(()),
    }
}

fn template_ready(run: &CommandRun<'_>) -> Result<(), BuildError> {
    run.log.push(LogLevel::Info, "Template is ready".to_owned());
    Ok(())
}

fn start_of(metadata: &TemplateMetadata, start_command: &str, ready_command: &str) -> Option<Start> {
    match start_command.is_empty() && ready_command.is_empty() {
        true => metadata.start.clone(),
        false => Some(Start {
            start_command: start_command.to_owned(),
            ready_command: ready_command.to_owned(),
            context: metadata.context.clone(),
        }),
    }
}

fn envd_defaults(context: &Context, context_defaults: bool) -> EnvdDefaults {
    match context_defaults {
        true => EnvdDefaults {
            user: context.user.clone(),
            workdir: context.workdir.clone(),
        },
        false => EnvdDefaults {
            user: Some(DEFAULT_USER.to_owned()),
            workdir: None,
        },
    }
}

#[derive(Debug, Clone, Default)]
struct BootedVm {
    template: TemplateVersions,
    vcpus: i64,
    cmdline_args: BTreeMap<String, String>,
    cpu_template: Option<CpuTemplate>,
    balloon: BalloonCaps,
}

fn final_metadata(source: &TemplateMetadata, start: Option<Start>, vm: BootedVm) -> TemplateMetadata {
    let cpu_template = vm.cpu_template.filter(|template| !template.is_empty());
    TemplateMetadata {
        start,
        template: vm.template,
        cmdline_args: vm.cmdline_args,
        balloon: Some(Balloon {
            reporting: vm.balloon.reporting,
            hinting: vm.balloon.hinting,
        }),
        cpu_template: cpu_template.clone(),
        build_cpu_template: cpu_template,
        ..source.clone()
    }
    .with_vcpu_count(vm.vcpus)
}

fn configure_script(template_id: &str, build_id: &str) -> String {
    CONFIGURE_SCRIPT
        .replace("{{ .TemplateID }}", template_id)
        .replace("{{ .BuildID }}", build_id)
}

fn ready_command(template_id: &str, start: &Start) -> String {
    match (start.ready_command.as_str(), start.start_command.as_str()) {
        ("", "") => NO_WAIT_READY_COMMAND.to_owned(),
        ("", _) => default_ready_command(template_id),
        (ready, _) => ready.to_owned(),
    }
}

fn default_ready_command(template_id: &str) -> String {
    let wait = match LONG_READY_TEMPLATES.contains(&template_id) {
        true => LONG_READY_WAIT_SECONDS,
        false => DEFAULT_READY_WAIT_SECONDS,
    };
    format!("sleep {wait}")
}

impl Phase for FinalizePhase {
    fn prefix(&self) -> String {
        FINALIZE.to_owned()
    }

    fn describe(&self) -> String {
        FINALIZE_TEXT.to_owned()
    }

    fn meta(&self) -> PhaseMeta {
        PhaseMeta {
            phase: FINALIZE,
            step_type: FINALIZE.to_owned(),
            step_number: None,
        }
    }

    fn hash<'a>(&'a self, source: &'a LayerResult) -> BoxFuture<'a, Result<String, BuildError>> {
        let hash = finalize_hash(&source.hash, CONFIGURE_SCRIPT, PACK_CERT_BUNDLE);
        async move { Ok(hash) }.boxed()
    }

    fn layer<'a>(&'a self, source: &'a LayerResult, hash: String) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move { Ok(self.final_layer(source, hash)) }.boxed()
    }

    fn build<'a>(
        &'a self,
        log: ScopedLog<'a>,
        source: &'a LayerResult,
        current: LayerResult,
    ) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move { self.build_final(&log, source, current).await }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(user: &str) -> Context {
        Context {
            user: Some(user.to_owned()),
            workdir: Some("/app".to_owned()),
            env_vars: None,
        }
    }

    fn start(start_command: &str, ready_command: &str) -> Start {
        Start {
            start_command: start_command.to_owned(),
            ready_command: ready_command.to_owned(),
            context: Context::default(),
        }
    }

    #[test]
    fn the_layer_records_the_vm_this_build_boots_not_the_cached_one() {
        for (name, cached, build) in [
            ("rebuilt larger", 2, 8),
            ("rebuilt smaller", 8, 2),
            ("unrecorded", 0, 4),
        ] {
            let source = TemplateMetadata {
                vcpu_count: cached,
                ..TemplateMetadata::default()
            };

            let vm = BootedVm {
                vcpus: build,
                ..BootedVm::default()
            };
            let metadata = final_metadata(&source, None, vm);

            assert_eq!(metadata.vcpu_count, build, "{name}");
        }
    }

    #[test]
    fn the_scripts_are_the_upstream_bytes() {
        assert_eq!(CONFIGURE_SCRIPT.lines().count(), 102);
        assert!(CONFIGURE_SCRIPT.starts_with("#!/bin/bash\nexport BASH_XTRACEFD=1\n"));
        assert!(CONFIGURE_SCRIPT.ends_with("echo \"Finished configuration script\"\n"));
        assert!(PACK_CERT_BUNDLE.starts_with("set -e\n# Recreate dpkg-recorded paths"));
        assert!(PACK_CERT_BUNDLE.contains("\n\tdpkg-query -W -f='${db:Status-Abbrev} ${binary:Package}\\n'"));
        assert!(PACK_CERT_BUNDLE.ends_with("tar -C /etc/ssl/certs -chf /usr/local/share/e2b/ssl-certs.tar .\n"));
    }

    #[test]
    fn the_configure_script_writes_the_requested_build_id() {
        let script = configure_script("tpl", "build-1");

        assert!(script.contains("cat <<EOF > /.e2b\nENV_ID=tpl\nTEMPLATE_ID=tpl\nBUILD_ID=build-1\nEOF\n"));
        assert!(!script.contains("{{"));
    }

    #[test]
    fn start_takes_the_current_context_only_when_a_command_is_set() {
        let inherited = Start {
            context: context("parent"),
            ..start("echo parent", "")
        };
        let metadata = TemplateMetadata {
            context: context("child"),
            start: Some(inherited.clone()),
            ..TemplateMetadata::default()
        };

        assert_eq!(start_of(&metadata, "", ""), Some(inherited));
        assert_eq!(
            start_of(&metadata, "", "sleep 1"),
            Some(Start {
                context: context("child"),
                ..start("", "sleep 1")
            })
        );
        assert_eq!(start_of(&TemplateMetadata::default(), "", ""), None);
    }

    #[test]
    fn envd_gets_the_context_defaults_from_v2_1_0() {
        let defaults = envd_defaults(&context("app"), true);
        assert_eq!(
            (defaults.user, defaults.workdir),
            (Some("app".to_owned()), Some("/app".to_owned()))
        );

        let defaults = envd_defaults(&context("app"), false);
        assert_eq!((defaults.user, defaults.workdir), (Some("user".to_owned()), None));
    }

    #[test]
    fn the_ready_command_defaults_like_go() {
        assert_eq!(ready_command("tpl", &start("", "")), "sleep 0");
        assert_eq!(ready_command("tpl", &start("serve", "")), "sleep 20");
        assert_eq!(ready_command("zegbt9dl3l2ixqem82mm", &start("serve", "")), "sleep 120");
        assert_eq!(
            ready_command("tpl", &start("serve", "curl -f localhost")),
            "curl -f localhost"
        );
        assert_eq!(ready_command("tpl", &start("", "true")), "true");
    }

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn tsc(khz: i64) -> CpuTemplate {
        CpuTemplate {
            x86_tsc_khz: khz,
            ..CpuTemplate::default()
        }
    }

    #[test]
    fn the_layer_stamps_this_builds_cmdline_args() {
        let cases = [
            (
                "this build wins",
                args(&[("psi", "1")]),
                args(&[("nokaslr", "")]),
                args(&[("nokaslr", "")]),
            ),
            (
                "none clears inherited ones",
                args(&[("psi", "1")]),
                args(&[]),
                args(&[]),
            ),
            ("nothing stays empty", args(&[]), args(&[]), args(&[])),
        ];
        for (name, inherited, build, want) in cases {
            let source = TemplateMetadata {
                cmdline_args: inherited,
                ..TemplateMetadata::default()
            };
            let vm = BootedVm {
                cmdline_args: build,
                ..BootedVm::default()
            };

            assert_eq!(final_metadata(&source, None, vm).cmdline_args, want, "{name}");
        }
    }

    #[test]
    fn the_layer_stamps_this_builds_cpu_template() {
        let cases = [
            (
                "this build wins",
                Some(tsc(3_200_000)),
                Some(tsc(2_400_000)),
                Some(tsc(2_400_000)),
            ),
            ("none clears an inherited one", Some(tsc(3_200_000)), None, None),
            ("nothing stays empty", None, None, None),
            (
                "an empty template is not recorded",
                None,
                Some(CpuTemplate::default()),
                None,
            ),
        ];
        for (name, inherited, build, want) in cases {
            let source = TemplateMetadata {
                cpu_template: inherited.clone(),
                build_cpu_template: inherited,
                ..TemplateMetadata::default()
            };
            let vm = BootedVm {
                cpu_template: build,
                ..BootedVm::default()
            };

            let metadata = final_metadata(&source, None, vm);

            assert_eq!(metadata.cpu_template, want, "{name}");
            assert_eq!(metadata.build_cpu_template, want, "{name}");
        }
    }

    #[test]
    fn the_layer_stamps_the_balloon_this_build_booted_with() {
        let source = TemplateMetadata {
            balloon: Some(Balloon {
                reporting: true,
                hinting: true,
            }),
            ..TemplateMetadata::default()
        };
        let vm = BootedVm {
            balloon: BalloonCaps {
                reporting: false,
                hinting: true,
            },
            ..BootedVm::default()
        };

        assert_eq!(
            final_metadata(&source, None, vm).balloon,
            Some(Balloon {
                reporting: false,
                hinting: true
            })
        );
    }

    #[test]
    fn the_balloon_is_written_like_go() {
        let json = serde_json::to_string(&Balloon::default()).unwrap();

        assert_eq!(json, r#"{"reporting":false,"hinting":false}"#);
    }
}
