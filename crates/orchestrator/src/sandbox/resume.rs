use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::Sandbox;
use super::config::SandboxBuilder;
use crate::block::build_file::BuildFile;
use crate::memory::handshake::UffdListener;
use crate::memory::server::MemoryServer;
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::system::all_ok;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

impl SandboxBuilder {
    pub async fn resume(self, build: &Build, storage: &Storage) -> Result<Sandbox, String> {
        let mut sandbox = Sandbox::new(self.build()?);
        match sandbox.start_resumed(build, storage).await {
            Ok(()) => Ok(sandbox),
            Err(e) => Err(sandbox.fail(e).await),
        }
    }
}

impl Sandbox {
    async fn start_resumed(&mut self, build: &Build, storage: &Storage) -> Result<(), String> {
        let memfile = self.config.memfile.clone().ok_or("memfile is required to resume")?;
        let snapfile = self.download_snapfile(build, storage).await?;
        let socket = self.config.work_dir.join("uffd.sock");
        let listener = UffdListener::bind(&socket).map_err(|e| format!("bind {}: {e}", socket.display()))?;
        let rootfs = self.attach_disk_and_slot().await?;
        let client = self.spawn_firecracker(rootfs).await?;
        let log = self.config.work_dir.join("firecracker.log");
        std::fs::File::create(&log).map_err(|e| format!("create {}: {e}", log.display()))?;
        client.set_logger(&log).await?;
        let (loaded, served) = tokio::join!(
            client.load_snapshot(&snapfile, &socket),
            serve_memory(listener, memfile)
        );
        let started = served.map(|server| {
            self.resources.memory = Some(server);
        });
        let resumed = async {
            all_ok([started, loaded])?;
            client.resume().await
        }
        .await;
        resumed.map_err(|e| format!("{e}; {}", self.resume_diagnostics(&log)))?;
        client.set_mmds(&self.mmds_metadata()).await?;
        self.init_envd().await
    }

    fn resume_diagnostics(&self, log: &std::path::Path) -> String {
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

    async fn download_snapfile(&self, build: &Build, storage: &Storage) -> Result<PathBuf, String> {
        let dir = &self.config.work_dir;
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| format!("create work dir {}: {e}", dir.display()))?;
        let bytes = storage.get(&build.paths().snapfile()).await?;
        let path = dir.join("snapfile");
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| format!("write {}: {e}", path.display()))
            .map(|()| path)
    }
}

async fn serve_memory(listener: UffdListener, memfile: Arc<BuildFile>) -> Result<MemoryServer, String> {
    let handshake = listener.accept(HANDSHAKE_TIMEOUT).await?;
    MemoryServer::start(handshake.uffd, handshake.regions, memfile).map_err(|e| format!("start memory server: {e}"))
}
