mod artifacts;
mod env;
mod fph_bench;
mod gdb;
mod guest;
mod optimize;
pub mod options;
mod report;
mod runner;
mod shell;

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use self::env::{Setup, setup_env};
use self::gdb::GdbLayout;
use self::options::{Options, RunOptions, apply_overrides, options};
use self::runner::{Runner, SandboxSettings};
use crate::featureflags::FeatureFlags;
use crate::server::app::App;
use crate::server::config::NodeConfig;
use crate::server::node::Node;
use crate::server::service::SandboxServer;
use crate::template::Template;
use crate::template::metadata::TemplateMetadata;

const EXIT_USAGE: i32 = 2;
const EXIT_FAILURE: i32 = 1;

pub async fn main_with(args: &[String]) -> i32 {
    let options = match options(args) {
        Ok(options) => options,
        Err(e) if e.starts_with("flag ") || e.starts_with("invalid ") => {
            eprintln!("{e}");
            eprintln!("Usage of resume-build:");
            options::FLAGS.iter().for_each(|(name, _)| eprintln!("  -{name}"));
            return EXIT_USAGE;
        }
        Err(e) => return fatal(&e),
    };
    if !nix::unistd::geteuid().is_root() {
        return fatal("run as root");
    }
    let setup = Setup {
        storage: &options.storage,
        sandbox_dir: &options.sandbox_dir,
        storage_explicit: options.storage_explicit,
    };
    if let Err(e) = setup_env(&setup) {
        return fatal(&e);
    }
    let cancel = CancellationToken::new();
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                println!("\n🛑 Stopping...");
                cancel.cancel();
            }
        }
    });
    match run(options, cancel.clone()).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e}");
            if cancel.is_cancelled() {
                eprintln!("(context: context canceled)");
            }
            EXIT_FAILURE
        }
    }
}

fn fatal(message: &str) -> i32 {
    eprintln!("{} {message}", chrono::Local::now().format("%Y/%m/%d %H:%M:%S"));
    EXIT_FAILURE
}

async fn run(mut options: Options, cancel: CancellationToken) -> Result<(), String> {
    install_logging(options.verbose);
    if options.gdb.enabled {
        options.no_egress = true;
    }
    let gdb = match options.gdb.enabled {
        true => Some(stage_gdb_versions()?),
        false => None,
    };
    if options.verbose {
        println!("🔧 Parsing config...");
    }
    let flags = apply_overrides(FeatureFlags::builder(), &options.overrides).offline();
    let config = NodeConfig::from_env()
        .map_err(|e| format!("config: {e}"))?
        .with_feature_flags(flags);
    crate::exposure::use_transport(config.rootfs_exposure).map_err(|e| format!("config: {e}"))?;
    let template_storage = config.template_storage.clone();
    let node = Arc::new(Node::new(&config).map_err(|e| format!("node: {e}"))?);
    if options.run.console {
        node.debug.lock().console = Some(console_printer());
    }
    App::new(node.clone(), None)?.start_background();
    let build_id = Uuid::parse_str(&options.from_build).map_err(|e| format!("invalid -from-build: {e}"))?;
    println!("📦 Loading {}...", options.from_build);
    let template = node.templates.get(build_id);
    let metadata = template.metadata().await.map_err(|e| format!("metadata: {e}"))?;
    print_template_info(&template, &metadata).await;
    if options.no_prefetch {
        println!("   Prefetch: disabled");
    }
    if options.force_reboot && !metadata.filesystem_only {
        println!(
            "⚠️  Forcing reboot of a memory-snapshot build: the disk is only crash-consistent — writes that lived in the guest page cache at pause time may be missing."
        );
    }
    let sandbox = sandbox_settings(&options, &metadata)?;
    let runner = Runner {
        server: SandboxServer::new(node.clone()),
        node,
        build_id: options.from_build.clone(),
        metadata,
        template_storage,
        sandbox,
        cold: options.cold,
        no_prefetch: options.no_prefetch,
        no_egress: options.no_egress,
        shell: options.shell,
        reboot: options.reboot,
        force_reboot: options.force_reboot,
        cancel,
    };
    runner.mask_prefetch().await;
    dispatch(&runner, &options, gdb.as_ref()).await
}

async fn dispatch(runner: &Runner, options: &Options, gdb: Option<&GdbLayout>) -> Result<(), String> {
    if let Some(layout) = gdb {
        let combined = options.fph_bench.enabled
            || !options.run.cmd.is_empty()
            || options.pause.enabled()
            || options.iterations > 0
            || options.reboot
            || options.force_reboot
            || options.shell;
        if combined {
            return Err(
                "-gdb cannot be combined with -pause/-cmd/-iterations/-reboot/-force-reboot/-shell/-fph-bench"
                    .to_owned(),
            );
        }
        return gdb::gdb_mode(runner, &options.gdb, layout).await;
    }
    if options.fph_bench.enabled {
        return fph_bench::fph_bench(runner, &options.fph_bench).await;
    }
    if !options.run.cmd.is_empty() {
        return runner.command_mode(&options.run).await;
    }
    if options.pause.enabled() {
        return runner.pause_mode(&options.pause).await;
    }
    if options.iterations > 0 {
        return runner.benchmark(options.iterations).await;
    }
    runner.interactive().await
}

fn sandbox_settings(options: &Options, metadata: &TemplateMetadata) -> Result<SandboxSettings, String> {
    let run = &options.run;
    let cold_boot = options.reboot || options.force_reboot;
    let recorded = metadata.vcpu_count;
    let firecracker_version = match run.firecracker_version.is_empty() {
        true => metadata.template.firecracker_version.clone(),
        false => {
            println!(
                "   Overriding Firecracker version: {} -> {}",
                metadata.template.firecracker_version, run.firecracker_version
            );
            run.firecracker_version.clone()
        }
    };
    println!("   envd: {}", run.envd_version.describe());
    let vcpu = match run.vcpu {
        0 => default_run_vcpu(recorded, cold_boot)?,
        vcpu => vcpu,
    };
    if run.vcpu == 0 && run.max_vcpus != 0 && run.max_vcpus < vcpu {
        return Err(format!(
            "-max-vcpus {} is below the snapshot's {vcpu} vcpus",
            run.max_vcpus
        ));
    }
    let (vm_vcpus, source) = vm_size(run, vcpu, recorded, cold_boot);
    println!("   vCPUs: {vcpu} of {vm_vcpus} ({source})");
    Ok(SandboxSettings {
        vcpu,
        max_vcpus: run.max_vcpus,
        kernel_version: metadata.template.kernel_version.clone(),
        firecracker_version,
        envd_version: run.envd_version.version().to_owned(),
    })
}

fn vm_size(run: &RunOptions, vcpu: i64, recorded: i64, cold_boot: bool) -> (i64, &'static str) {
    match (cold_boot, recorded) {
        (true, _) => {
            let size = vcpu.max(run.max_vcpus);
            let size = match run.max_vcpus {
                0 => size.max(recorded),
                _ => size,
            };
            (size, "cold-boot VM")
        }
        (false, 0) => (vcpu.max(run.max_vcpus), "inferred from flags; snapshot size unrecorded"),
        (false, recorded) => (recorded, "recorded in the snapshot"),
    }
}

pub fn default_run_vcpu(recorded: i64, cold_boot: bool) -> Result<i64, String> {
    match (recorded, cold_boot) {
        (recorded, _) if recorded > 0 => Ok(recorded),
        (_, true) => Ok(1),
        _ => Err("snapshot size unrecorded; pass -vcpu for a memory resume".to_owned()),
    }
}

async fn print_template_info(template: &Template, metadata: &TemplateMetadata) {
    println!(
        "   Kernel: {}, Firecracker: {}",
        metadata.template.kernel_version, metadata.template.firecracker_version
    );
    if let Ok(build) = template.build().await {
        [("Memfile", &build.memfile), ("Rootfs", &build.rootfs)]
            .iter()
            .filter(|(_, header)| header.metadata.size > 0)
            .for_each(|(name, header)| {
                println!(
                    "   {name}: {} MB ({} KB blocks)",
                    header.metadata.size >> 20,
                    header.metadata.block_size >> 10
                )
            });
    }
    if let Some(prefetch) = metadata.memory_prefetch() {
        println!("   Prefetch: {} blocks", prefetch.indices.len());
    }
}

fn stage_gdb_versions() -> Result<GdbLayout, String> {
    let original = std::env::var("FIRECRACKER_VERSIONS_DIR").unwrap_or_else(|_| "/fc-versions".to_owned());
    let staging = tempfile::Builder::new()
        .prefix("fc-gdb-versions-")
        .tempdir()
        .map_err(|e| format!("gdb fc staging dir: {e}"))?
        .keep();
    unsafe { std::env::set_var("FIRECRACKER_VERSIONS_DIR", &staging) };
    Ok(GdbLayout {
        original_versions_dir: PathBuf::from(original),
        staging_versions_dir: staging,
        kernels_dir: PathBuf::from(std::env::var("HOST_KERNELS_DIR").unwrap_or_else(|_| "/fc-kernels".to_owned())),
        arch: crate::builder::config::target_arch(std::env::var("TARGET_ARCH").ok().as_deref()),
    })
}

fn console_printer() -> tokio::sync::mpsc::UnboundedSender<String> {
    let (lines, mut received) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = received.recv().await {
            println!("{line}");
        }
    });
    lines
}

fn install_logging(verbose: bool) {
    let level = match verbose {
        true => tracing_subscriber::filter::LevelFilter::INFO,
        false => tracing_subscriber::filter::LevelFilter::OFF,
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_vcpu_is_the_recorded_size_or_one_for_a_cold_boot() {
        assert_eq!(default_run_vcpu(4, false), Ok(4));
        assert_eq!(default_run_vcpu(0, true), Ok(1));
        assert_eq!(
            default_run_vcpu(0, false),
            Err("snapshot size unrecorded; pass -vcpu for a memory resume".to_owned())
        );
    }

    #[test]
    fn the_vm_size_follows_go() {
        let run = RunOptions::default();

        assert_eq!(vm_size(&run, 2, 4, false), (4, "recorded in the snapshot"));
        assert_eq!(
            vm_size(&run, 2, 0, false),
            (2, "inferred from flags; snapshot size unrecorded")
        );
        assert_eq!(vm_size(&run, 2, 4, true), (4, "cold-boot VM"));
        let capped = RunOptions {
            max_vcpus: 3,
            ..RunOptions::default()
        };
        assert_eq!(vm_size(&capped, 2, 8, true), (3, "cold-boot VM"));
    }
}
