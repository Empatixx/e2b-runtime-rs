use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use tokio::sync::oneshot;

use super::Sandbox;
use super::config::{SandboxBuilder, sandbox_builder};
use super::vcpu_limit::{limit_vcpus_before_resume, resolve_vm_vcpus, resume_vm_vcpus};
use crate::block::build_file::BuildFile;
use crate::block::overlay::Overlay;
use crate::exposure::nbd::{Exposure, expose};
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::process::FirecrackerProcess;
use crate::firecracker::vcpus::{exceeds_snapshot_error, vcpu_cap_resume_error};
use crate::memory::handshake::UffdListener;
use crate::memory::prefetch::plan::{PrefetchPlan, PrefetchSummary};
use crate::memory::prefetch::{PrefetchStats, fetch_only, prefetch};
use crate::memory::server::{MemoryServer, MemoryWatch, Prefaulter};
use crate::memory::trace::Trace;
use crate::system::all_ok;
use crate::telemetry::report::report_error;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const NO_DEVICE: &str = "/dev/null";

impl<S: sandbox_builder::IsComplete> SandboxBuilder<S> {
    pub async fn resume(self, snapfile: &Path) -> Result<Sandbox, String> {
        let mut sandbox = Sandbox::new(self.build());
        match sandbox.start_resumed(snapfile).await {
            Ok(()) => Ok(sandbox),
            Err(e) => Err(sandbox.fail(e).await),
        }
    }
}

impl Sandbox {
    async fn start_resumed(&mut self, snapfile: &Path) -> Result<(), String> {
        self.record_envd_defaults();
        self.resolved_vm_vcpus = resume_vm_vcpus(
            i64::from(self.config.vcpus),
            i64::from(self.config.max_vcpus),
            self.config.recorded_vm_vcpus,
        )?;
        self.create_cgroup(self.resolved_vm_vcpus > i64::from(self.config.vcpus))?;
        let memfile = self.config.memfile.clone().ok_or("memfile is required to resume")?;
        std::fs::create_dir_all(&self.config.work_dir)
            .map_err(|e| format!("create work dir {}: {e}", self.config.work_dir.display()))?;
        let socket = self.config.work_dir.join("uffd.sock");
        let listener = UffdListener::bind(&socket).map_err(|e| format!("bind {}: {e}", socket.display()))?;
        let prefaulter = self.start_prefetch(memfile.clone());
        let client = self.launch_with_rootfs().await?;
        self.phases.mark("rootfs+firecracker");
        let loading = client.load_snapshot(snapfile, &socket, self.config.use_memfd);
        let serving = serve_memory(listener, memfile, prefaulter);
        tokio::pin!(loading, serving);
        let (loaded, served) = tokio::select! {
            loaded = &mut loading => match loaded {
                Err(e) => (Err(e), Err("firecracker never connected to the memory server".to_owned())),
                loaded => (loaded, serving.await),
            },
            served = &mut serving => (loading.await, served),
        };
        let started = served.map(|server| {
            self.resources.memory = Some(server);
        });
        all_ok([started, loaded]).map_err(|e| format!("{e}; {}", self.resume_diagnostics()))?;
        self.phases.mark("load_snapshot");
        self.phases.note("faults", self.faults_served());
        self.phases.note(
            "memfd",
            self.resources
                .memory
                .as_ref()
                .is_some_and(|memory| memory.memfd().is_some()),
        );
        client.set_rate_limits().await?;
        self.cap_vcpus_before_resume()?;
        client
            .resume()
            .await
            .map_err(|e| format!("{e}; {}", self.resume_diagnostics()))?;
        self.phases.mark("resume_vm");
        client.set_mmds(&self.mmds_metadata()).await?;
        self.phases.mark("mmds");
        self.wait_for_envd_unless_memory_fails().await?;
        self.phases.mark("envd_init");
        self.phases.note("faults", self.faults_served());
        self.phases.note("prefetched", self.prefetch_stats.installed());
        Ok(())
    }

    fn cap_vcpus_before_resume(&mut self) -> Result<(), String> {
        let pid = self.resources.vm.as_ref().and_then(FirecrackerProcess::pid);
        let recorded = self.config.recorded_vm_vcpus > 0;
        let target = i64::from(self.config.vcpus);
        let snapshot = pid.map_or(self.resolved_vm_vcpus, |pid| {
            resolve_vm_vcpus(pid, &self.config.id, self.resolved_vm_vcpus, recorded)
        });
        self.resolved_vm_vcpus = snapshot;
        let capped = limit_vcpus_before_resume(self.resources.cgroup.as_mut(), &self.config.id, target, snapshot);
        vcpu_cap_resume_error(capped.err(), self.config.max_vcpus > 0)
            .map_err(|e| format!("failed to start FC: {e}"))?;
        if snapshot < target {
            report_error(
                "resumed with fewer vcpus than requested",
                Some(&exceeds_snapshot_error(snapshot, target)),
                &[KeyValue::new("sandbox.id", self.config.id.clone())],
            );
        }
        Ok(())
    }

    async fn wait_for_envd_unless_memory_fails(&mut self) -> Result<(), String> {
        let watch = self.resources.memory.as_ref().map(MemoryServer::watch);
        tokio::select! {
            initialised = self.wait_for_envd() => initialised,
            error = memory_failure(watch) => Err(format!("uffd process exited: {error}; {}", self.resume_diagnostics())),
        }
    }

    fn start_prefetch(&mut self, memfile: Arc<BuildFile>) -> Option<oneshot::Sender<Arc<Prefaulter>>> {
        let plan = PrefetchPlan::resolve(
            self.config.prefetch_settings,
            self.config.prefetch.clone(),
            memfile.header(),
        );
        tracing::debug!(
            sandbox = %self.config.id,
            init_blocks = plan.init_blocks(),
            last_cycle_blocks = plan.last_cycle_blocks(),
            "resolved the resume prefetch"
        );
        self.prefetch_summary = Some(plan.summary(self.config.prefetch_settings.source));
        if plan.is_empty() {
            return None;
        }
        let (prefaulter, installer) = oneshot::channel();
        let stats = self.prefetch_stats.clone();
        self.resources.prefetch = Some(tokio::spawn(replay(plan, memfile, installer, stats)));
        Some(prefaulter)
    }

    pub fn prefetch_summary(&self) -> Option<PrefetchSummary> {
        self.prefetch_summary
    }

    pub fn prefetched(&self) -> u64 {
        self.prefetch_stats.installed()
    }

    pub fn memory_trace(&self) -> Option<Trace> {
        self.resources.memory.as_ref().map(MemoryServer::trace)
    }

    async fn launch_with_rootfs(&mut self) -> Result<FirecrackerClient, String> {
        let link = self.config.work_dir.join("rootfs.link");
        point_link(&link, Path::new(NO_DEVICE)).map_err(|e| format!("link {}: {e}", link.display()))?;
        let overlay = self.config.overlay()?;
        let (disk, client) = tokio::join!(expose_apart(overlay), self.spawn_in_slot(link.clone()));
        let device = self.keep_disk(disk)?;
        let client = client?;
        point_link(&link, &device).map_err(|e| format!("link {} to {}: {e}", link.display(), device.display()))?;
        Ok(client)
    }

    async fn spawn_in_slot(&mut self, rootfs: PathBuf) -> Result<FirecrackerClient, String> {
        self.own_slot().await?;
        self.spawn_firecracker(rootfs).await
    }

    fn keep_disk(&mut self, disk: io::Result<Exposure>) -> Result<PathBuf, String> {
        disk.map(|exposure| self.resources.disk.insert(exposure).path())
            .map_err(|e| format!("expose rootfs: {e}"))
    }

    fn resume_diagnostics(&self) -> String {
        format!(
            "faults served: {}; memory server running: {}; console: {}",
            self.faults_served(),
            self.resources
                .memory
                .as_ref()
                .is_some_and(|memory| !memory.is_finished()),
            self.console_tail()
        )
    }
}

fn point_link(link: &Path, target: &Path) -> io::Result<()> {
    let staged = link.with_extension("staged");
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(target, &staged)?;
    std::fs::rename(&staged, link)
}

async fn replay(
    plan: PrefetchPlan,
    memfile: Arc<BuildFile>,
    installer: oneshot::Receiver<Arc<Prefaulter>>,
    stats: Arc<PrefetchStats>,
) -> Result<(), String> {
    if let Some(init) = plan.init {
        let _ = prefetch(
            init,
            memfile.clone(),
            async { installer.await.ok() },
            stats.clone(),
            plan.workers,
        )
        .await
        .inspect_err(|e| tracing::error!(error = %e, "failed to start init prefetcher"));
    }
    if let Some(last_cycle) = plan.last_cycle {
        fetch_only(last_cycle, memfile, stats, plan.workers).await;
    }
    Ok(())
}

async fn memory_failure(watch: Option<MemoryWatch>) -> String {
    match watch {
        Some(watch) => watch.failed().await,
        None => std::future::pending().await,
    }
}

async fn expose_apart(overlay: Arc<Overlay>) -> io::Result<Exposure> {
    tokio::spawn(expose(overlay))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
}

async fn serve_memory(
    listener: UffdListener,
    memfile: Arc<BuildFile>,
    prefaulter: Option<oneshot::Sender<Arc<Prefaulter>>>,
) -> Result<MemoryServer, String> {
    let handshake = listener.accept(HANDSHAKE_TIMEOUT).await?;
    let server = MemoryServer::start(handshake.uffd, handshake.regions, memfile, handshake.memfd)
        .map_err(|e| format!("start memory server: {e}"))?;
    if let Some(prefaulter) = prefaulter {
        let _ = prefaulter.send(server.prefaulter());
    }
    Ok(server)
}
