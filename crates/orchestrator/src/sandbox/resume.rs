use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use super::Sandbox;
use super::config::SandboxBuilder;
use crate::block::build_file::BuildFile;
use crate::exposure::nbd::{Exposure, expose};
use crate::firecracker::client::FirecrackerClient;
use crate::memory::handshake::UffdListener;
use crate::memory::prefetch::prefetch;
use crate::memory::server::{MemoryServer, Prefaulter};
use crate::memory::trace::Trace;
use crate::system::all_ok;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const NO_DEVICE: &str = "/dev/null";

impl SandboxBuilder {
    pub async fn resume(self, snapfile: &Path) -> Result<Sandbox, String> {
        let mut sandbox = Sandbox::new(self.build()?);
        match sandbox.start_resumed(snapfile).await {
            Ok(()) => Ok(sandbox),
            Err(e) => Err(sandbox.fail(e).await),
        }
    }
}

impl Sandbox {
    async fn start_resumed(&mut self, snapfile: &Path) -> Result<(), String> {
        let memfile = self.config.memfile.clone().ok_or("memfile is required to resume")?;
        std::fs::create_dir_all(&self.config.work_dir)
            .map_err(|e| format!("create work dir {}: {e}", self.config.work_dir.display()))?;
        let socket = self.config.work_dir.join("uffd.sock");
        let listener = UffdListener::bind(&socket).map_err(|e| format!("bind {}: {e}", socket.display()))?;
        let prefaulter = self.start_prefetch(memfile.clone());
        let client = self.launch_with_rootfs().await?;
        self.phases.mark("rootfs+firecracker");
        let (loaded, served) = tokio::join!(
            client.load_snapshot(snapfile, &socket, self.config.use_memfd),
            serve_memory(listener, memfile)
        );
        let started = served.map(|server| {
            if let Some(prefaulter) = prefaulter {
                let _ = prefaulter.send(server.prefaulter());
            }
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
        client
            .resume()
            .await
            .map_err(|e| format!("{e}; {}", self.resume_diagnostics()))?;
        self.phases.mark("resume_vm");
        client.set_mmds(&self.mmds_metadata()).await?;
        self.phases.mark("mmds");
        self.init_envd().await?;
        self.phases.mark("envd_init");
        self.phases.note("faults", self.faults_served());
        self.phases.note("prefetched", self.prefetch_stats.installed());
        Ok(())
    }

    fn start_prefetch(&mut self, memfile: Arc<BuildFile>) -> Option<oneshot::Sender<Arc<Prefaulter>>> {
        let mapping = self.config.prefetch.clone()?;
        let (prefaulter, installer) = oneshot::channel();
        let stats = self.prefetch_stats.clone();
        self.resources.prefetch = Some(tokio::spawn(async move {
            prefetch(mapping, memfile, async { installer.await.ok() }, stats)
                .await
                .inspect_err(|e| tracing::warn!(error = %e, "memory prefetch failed"))
        }));
        Some(prefaulter)
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
        let (disk, client) = tokio::join!(expose(self.config.rootfs.clone()), self.spawn_in_slot(link.clone()));
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

async fn serve_memory(listener: UffdListener, memfile: Arc<BuildFile>) -> Result<MemoryServer, String> {
    let handshake = listener.accept(HANDSHAKE_TIMEOUT).await?;
    MemoryServer::start(handshake.uffd, handshake.regions, memfile, handshake.memfd)
        .map_err(|e| format!("start memory server: {e}"))
}
