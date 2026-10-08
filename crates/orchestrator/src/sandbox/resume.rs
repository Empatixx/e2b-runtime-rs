use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::Sandbox;
use super::config::SandboxBuilder;
use crate::block::build_file::BuildFile;
use crate::memory::handshake::UffdListener;
use crate::memory::server::MemoryServer;
use crate::system::all_ok;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
        let rootfs = self.attach_disk_and_slot().await?;
        self.phases.mark("slot+rootfs");
        let client = self.spawn_firecracker(rootfs).await?;
        self.phases.mark("firecracker");
        let log = self.config.work_dir.join("firecracker.log");
        std::fs::File::create(&log).map_err(|e| format!("create {}: {e}", log.display()))?;
        client.set_logger(&log).await?;
        let (loaded, served) = tokio::join!(client.load_snapshot(snapfile, &socket), serve_memory(listener, memfile));
        let started = served.map(|server| {
            self.resources.memory = Some(server);
        });
        self.phases.mark("load_snapshot");
        let resumed = async {
            all_ok([started, loaded])?;
            client.resume().await
        }
        .await;
        resumed.map_err(|e| format!("{e}; {}", self.resume_diagnostics(&log)))?;
        self.phases.mark("resume_vm");
        client.set_mmds(&self.mmds_metadata()).await?;
        self.phases.mark("mmds");
        self.init_envd().await?;
        self.phases.mark("envd_init");
        Ok(())
    }

    fn resume_diagnostics(&self, log: &Path) -> String {
        let firecracker_log = std::fs::read_to_string(log).unwrap_or_default();
        let start = firecracker_log.len().saturating_sub(4000);
        format!(
            "faults served: {}; memory server running: {}; firecracker log: {}; console: {}",
            self.faults_served(),
            self.resources
                .memory
                .as_ref()
                .is_some_and(|memory| !memory.is_finished()),
            firecracker_log.get(start..).unwrap_or_default(),
            self.console_tail()
        )
    }
}

async fn serve_memory(listener: UffdListener, memfile: Arc<BuildFile>) -> Result<MemoryServer, String> {
    let handshake = listener.accept(HANDSHAKE_TIMEOUT).await?;
    MemoryServer::start(handshake.uffd, handshake.regions, memfile).map_err(|e| format!("start memory server: {e}"))
}
