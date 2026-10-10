pub mod config;
pub mod harvest;
pub mod reboot;
pub mod resources;
pub mod resume;
pub mod snapshot;
pub mod upgrade;
pub mod vcpu_limit;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use self::config::{Rootfs, SandboxBuilder, SandboxConfig, sandbox_builder};
use self::resources::Resources;
use self::vcpu_limit::{boot_cpu_quota, guest_cpu_count, move_limit_to_vcpu_threads, record_boot_failure};
use crate::builder::config::host_arch;
use crate::cgroup::Cgroup;
use crate::cgroup::cpu_limit::set_sandbox_cpu_limit;
use crate::envd::cpus::verify_envd_cpu_count;
use crate::envd::report::EnvdReport;
use crate::envd::{self, ENVD_PORT};
use crate::exposure::nbd::expose;
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::config::VmConfig;
use crate::firecracker::exit::{CrashCause, ExitInfo};
use crate::firecracker::launch::{GUEST_KERNEL, GUEST_ROOTFS, Launch};
use crate::firecracker::mmds::MmdsMetadata;
use crate::firecracker::process::FirecrackerProcess;
use crate::memory::prefetch::PrefetchStats;
use crate::memory::prefetch::plan::PrefetchSummary;
use crate::phases::PhaseTimer;
use crate::telemetry::report::report_error;

pub struct Death {
    pub reason: String,
    pub cause: CrashCause,
    pub exit: Option<ExitInfo>,
}

pub struct Sandbox {
    config: SandboxConfig,
    resources: Resources,
    phases: PhaseTimer,
    prefetch_stats: Arc<PrefetchStats>,
    prefetch_summary: Option<PrefetchSummary>,
    resolved_vm_vcpus: i64,
    reinit: parking_lot::Mutex<Option<tokio_util::sync::DropGuard>>,
    envd_report: parking_lot::Mutex<EnvdReport>,
}

impl<S: sandbox_builder::IsComplete> SandboxBuilder<S> {
    pub async fn cold_boot(self) -> Result<Sandbox, String> {
        let config = self.build();
        config.kernel.as_ref().ok_or("kernel is required")?;
        let mut sandbox = Sandbox::new(config);
        match sandbox.start().await {
            Ok(()) => Ok(sandbox),
            Err(e) => Err(sandbox.fail(e).await),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let leftovers = std::mem::take(&mut self.resources);
        if !leftovers.is_empty() {
            tracing::warn!(sandbox = %self.config.id, "sandbox dropped without stop; releasing in the background");
            leftovers.release_in_background(self.config.id.clone());
        }
    }
}

impl Sandbox {
    pub fn builder() -> SandboxBuilder {
        SandboxConfig::builder()
    }

    pub fn id(&self) -> &str {
        &self.config.id
    }

    pub fn envd_address(&self) -> SocketAddr {
        SocketAddr::from((self.config.slot.host_ip(), ENVD_PORT))
    }

    pub fn cgroup_path(&self) -> Option<&Path> {
        self.resources.cgroup.as_ref().map(Cgroup::path)
    }

    pub fn cgroup_stats(&self) -> Option<crate::cgroup::CgroupStats> {
        self.resources.cgroup.as_ref().map(Cgroup::stats)
    }

    pub fn phases(&self) -> String {
        self.phases.render()
    }

    pub fn faults_served(&self) -> u64 {
        self.resources
            .memory
            .as_ref()
            .map_or(0, |memory| memory.faults_served())
    }

    pub async fn died(&mut self) -> Death {
        let Resources { vm, memory, disk, .. } = &mut self.resources;
        tokio::select! {
            exited = async { match vm.as_mut() { Some(vm) => vm.exited().await, None => std::future::pending().await } } => Death {
                cause: CrashCause::classify(exited.info.as_ref()),
                reason: exited.message,
                exit: exited.info,
            },
            () = async { match memory.as_ref() { Some(memory) => memory.ended().await, None => std::future::pending().await } } => Death {
                reason: "the memory server stopped".to_owned(),
                cause: CrashCause::RequestedSignal.with_memory_handler(true),
                exit: None,
            },
            () = async { match disk.as_ref() { Some(disk) => disk.ended().await, None => std::future::pending().await } } => Death {
                reason: "the rootfs nbd server stopped".to_owned(),
                cause: CrashCause::RequestedSignal,
                exit: None,
            },
        }
    }

    pub async fn stop(mut self) -> Result<(), String> {
        std::mem::take(&mut self.resources).release().await
    }

    fn new(mut config: SandboxConfig) -> Self {
        let resolved_vm_vcpus = i64::from(config.vcpus).max(i64::from(config.max_vcpus));
        Self {
            resolved_vm_vcpus,
            resources: Resources {
                held: std::mem::take(&mut config.held),
                ..Resources::default()
            },
            config,
            phases: PhaseTimer::start(),
            prefetch_stats: Arc::default(),
            prefetch_summary: None,
            reinit: parking_lot::Mutex::default(),
            envd_report: parking_lot::Mutex::default(),
        }
    }

    async fn fail(mut self, error: String) -> String {
        match std::mem::take(&mut self.resources).release().await {
            Ok(()) => error,
            Err(teardown) => format!("{error}; teardown: {teardown}"),
        }
    }

    pub async fn shut_down(mut self) -> Result<(), String> {
        let client = self.resources.vm.as_ref().ok_or("firecracker is not running")?.client();
        client.pause().await.map_err(|e| format!("failed to pause VM: {e}"))?;
        let snapfile = self.config.work_dir.join("shutdown.snapfile");
        let snapshot = client.create_snapshot(&snapfile).await;
        let _ = std::fs::remove_file(&snapfile);
        snapshot.map_err(|e| format!("error creating snapshot: {e}"))?;
        let rootfs = self.config.rootfs.clone();
        std::mem::take(&mut self.resources)
            .release()
            .await
            .map_err(|e| format!("error stopping sandbox: {e}"))?;
        match rootfs {
            Rootfs::File(path) => sync_file(path)
                .await
                .map_err(|e| format!("error stopping sandbox: {e}")),
            Rootfs::Overlay(_) => Ok(()),
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        let target_vcpus = i64::from(self.config.vcpus);
        let cpu_quota = boot_cpu_quota(target_vcpus, self.resolved_vm_vcpus);
        self.create_cgroup(cpu_quota > 0)?;
        let rootfs = self.attach_disk_and_slot().await?;
        if let Some(hook) = self.config.pre_boot.take() {
            hook(rootfs.clone())
                .await
                .map_err(|e| format!("pre-boot hook failed: {e}"))?;
        }
        self.limit_cgroup_before_boot(cpu_quota)?;
        let client = self.spawn_firecracker(rootfs).await?;
        self.boot(&client).await?;
        move_limit_to_vcpu_threads(
            self.resources.cgroup.as_mut(),
            &self.config.id,
            cpu_quota,
            self.resolved_vm_vcpus,
        );
        match self.config.wait_for_envd {
            true => self.wait_for_envd().await,
            false => Ok(()),
        }
    }

    async fn attach_disk_and_slot(&mut self) -> Result<PathBuf, String> {
        let overlay = match &self.config.rootfs {
            Rootfs::Overlay(overlay) => overlay.clone(),
            Rootfs::File(path) => {
                let path = path.clone();
                return self.own_slot().await.map(|()| path);
            }
        };
        let (disk, created) = tokio::join!(expose(overlay), self.own_slot());
        let exposed = disk
            .map(|exposure| self.resources.disk.insert(exposure).path())
            .map_err(|e| format!("expose rootfs: {e}"));
        created.and(exposed)
    }

    async fn own_slot(&mut self) -> Result<(), String> {
        if self.config.slot_ready {
            return Ok(());
        }
        let slot = *self.resources.slot.insert(self.config.slot);
        slot.create().await
    }

    async fn spawn_firecracker(&mut self, rootfs: PathBuf) -> Result<FirecrackerClient, String> {
        std::fs::create_dir_all(&self.config.work_dir)
            .map_err(|e| format!("create work dir {}: {e}", self.config.work_dir.display()))?;
        let cgroup = self.resources.cgroup.as_ref().map(|cgroup| cgroup.path().to_owned());
        let launch = Launch::builder()
            .binary(&self.config.firecracker)
            .socket(self.config.work_dir.join("firecracker.sock"))
            .namespace(self.config.slot.namespace())
            .rootfs(rootfs)
            .kernel(self.config.kernel.clone().unwrap_or_else(|| PathBuf::from("/dev/null")))
            .maybe_cgroup(cgroup)
            .build();
        let vm = FirecrackerProcess::spawn(&launch).await?;
        if let Some(console) = self.config.console.clone() {
            forward_console(vm.console_lines(), console);
        }
        Ok(self.resources.vm.insert(vm).client())
    }

    fn create_cgroup(&mut self, needs_limit: bool) -> Result<(), String> {
        match Cgroup::create(&self.config.cgroup_name) {
            Ok(cgroup) => {
                self.resources.cgroup = Some(cgroup);
                Ok(())
            }
            Err(e) if needs_limit => Err(format!("failed to create cgroup for the vcpu limit: {e}")),
            Err(e) => {
                tracing::warn!(sandbox = %self.config.id, error = %e, "could not create the sandbox cgroup");
                Ok(())
            }
        }
    }

    fn limit_cgroup_before_boot(&mut self, cpu_quota: i64) -> Result<(), String> {
        if cpu_quota <= 0 {
            return Ok(());
        }
        set_sandbox_cpu_limit(self.resources.cgroup.as_ref(), cpu_quota).map_err(|e| {
            record_boot_failure();
            format!("error limiting sandbox cgroup before boot: {e}")
        })?;
        self.config.boot.boot_cpus = i64::from(self.config.vcpus);
        Ok(())
    }

    async fn boot(&self, client: &FirecrackerClient) -> Result<(), String> {
        let vm_config = VmConfig::builder()
            .kernel(GUEST_KERNEL)
            .rootfs(GUEST_ROOTFS)
            .vcpus(self.resolved_vm_vcpus as u32)
            .memory_mib(self.config.memory_mib)
            .boot_args(self.config.boot.kernel_args(host_arch()))
            .maybe_io_engine(self.config.io_engine.as_deref())
            .huge_pages(self.config.huge_pages)
            .maybe_cpu_template(self.config.cpu_template.clone())
            .build();
        let metadata = self.config.mmds.then(|| self.mmds_metadata());
        client.boot(&vm_config, metadata.as_ref()).await
    }

    fn mmds_metadata(&self) -> MmdsMetadata {
        MmdsMetadata {
            sandbox_id: self.config.id.clone(),
            template_id: self.config.template_id.clone(),
            logs_collector_address: self
                .config
                .hyperloop_ip
                .as_ref()
                .map(|ip| format!("http://{ip}/logs"))
                .unwrap_or_default(),
            access_token_hash: Some(envd::hash_access_token(
                self.config.envd.access_token.as_deref().unwrap_or_default(),
            )),
        }
    }

    pub async fn wait_for_envd(&mut self) -> Result<(), String> {
        self.wait_for_envd_within(self.config.envd_timeout).await
    }

    pub async fn wait_for_envd_within(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let address = self.envd_address();
        let settings = self.config.envd.with_cpu_count(self.guest_cpu_count());
        let vm = self.resources.vm.as_mut().ok_or("firecracker is not running")?;
        let initialised = tokio::select! {
            initialised = envd::init(address, &settings, deadline) => initialised,
            exited = vm.exited() => return Err(exited.message),
        };
        let initialised = initialised.map_err(|e| format!("{e}; console: {}", self.console_tail()))?;
        self.phases.note("envd_attempts", initialised.attempts);
        self.verify_cpu_count(&initialised.cpus_header);
        self.keep_report(initialised.report);
        Ok(())
    }

    fn guest_cpu_count(&self) -> i64 {
        guest_cpu_count(self.resolved_vm_vcpus, i64::from(self.config.vcpus))
    }

    fn verify_cpu_count(&self, header: &str) {
        verify_envd_cpu_count(header, self.guest_cpu_count(), self.resolved_vm_vcpus)
            .err()
            .iter()
            .for_each(|e| {
                report_error(
                    "envd did not take the cpu count",
                    Some(e.as_str()),
                    &[
                        KeyValue::new("sandbox.id", self.config.id.clone()),
                        KeyValue::new("envd.version", self.config.envd_version.clone()),
                    ],
                )
            });
    }

    fn console_tail(&self) -> String {
        self.resources
            .vm
            .as_ref()
            .map(FirecrackerProcess::console_tail)
            .unwrap_or_default()
    }
}

fn forward_console(mut lines: UnboundedReceiver<String>, sink: UnboundedSender<String>) {
    tokio::spawn(async move {
        while let Some(line) = lines.recv().await {
            if sink.send(line).is_err() {
                return;
            }
        }
    });
}

async fn sync_file(path: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || std::fs::File::open(&path).and_then(|file| file.sync_all()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}
