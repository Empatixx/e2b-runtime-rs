#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), String> {
    linux::run().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("cold_boot runs on Linux only");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
    use orchestrator::block::overlay::Overlay;
    use orchestrator::envd::{ENVD_PORT, EnvdInit};
    use orchestrator::network::slot::Slot;
    use orchestrator::sandbox::Sandbox;
    use orchestrator::storage::backend::Storage;
    use orchestrator::storage::header::{Header, Metadata};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::signal::unix::{SignalKind, signal};
    use uuid::Uuid;

    const SLOT: u32 = 4020;

    struct Assets {
        firecracker: PathBuf,
        kernel: PathBuf,
        rootfs: PathBuf,
    }

    fn assets() -> Result<Assets, String> {
        let env = |name: &str| {
            std::env::var(name)
                .map(PathBuf::from)
                .map_err(|_| format!("{name} is not set"))
        };
        Ok(Assets {
            firecracker: env("E2B_FIRECRACKER")?,
            kernel: env("E2B_KERNEL")?,
            rootfs: env("E2B_ROOTFS")?,
        })
    }

    pub async fn run() -> Result<(), String> {
        let assets = assets()?;
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let template_id = Uuid::new_v4();
        let rootfs = template_overlay(&assets.rootfs, dir.path(), template_id).await?;
        let slot = Slot::new(SLOT)?;
        let _ = slot.destroy().await;
        let listener = TcpListener::bind(("127.0.0.1", ENVD_PORT))
            .await
            .map_err(|e| format!("bind 127.0.0.1:{ENVD_PORT}: {e}"))?;

        let sandbox = Sandbox::builder()
            .id("sbx-cold-boot")
            .template_id(&template_id.to_string())
            .rootfs(rootfs)
            .slot(slot)
            .firecracker(&assets.firecracker)
            .kernel(&assets.kernel)
            .work_dir(dir.path().join("sandbox"))
            .envd(EnvdInit {
                default_user: Some("user".to_owned()),
                default_workdir: Some("/home/user".to_owned()),
                ..EnvdInit::default()
            })
            .cold_boot()
            .await?;
        println!("ready {}", sandbox.envd_address());

        let forwarding = tokio::spawn(forward(listener, sandbox.envd_address()));
        shutdown_requested().await?;
        forwarding.abort();
        sandbox.stop().await
    }

    async fn template_overlay(rootfs: &Path, dir: &Path, template_id: Uuid) -> Result<Arc<Overlay>, String> {
        let storage = Storage::local(&dir.join("store"))?;
        storage.put_file(&Artifact::Rootfs.object(template_id), rootfs).await?;
        let size = std::fs::metadata(rootfs)
            .map_err(|e| format!("stat {}: {e}", rootfs.display()))?
            .len();
        let header = Header::new(Metadata::template(template_id, 4096, size), Vec::new())?;
        let store = Arc::new(DiffStore::new(storage, dir.join("cache")));
        let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
        Overlay::new(base, &dir.join("overlay"))
            .map(Arc::new)
            .map_err(|e| format!("create overlay: {e}"))
    }

    async fn forward(listener: TcpListener, guest: SocketAddr) {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                if let Ok(mut upstream) = TcpStream::connect(guest).await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
            });
        }
    }

    async fn shutdown_requested() -> Result<(), String> {
        let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = terminate.recv() => Ok(()),
        }
    }
}
