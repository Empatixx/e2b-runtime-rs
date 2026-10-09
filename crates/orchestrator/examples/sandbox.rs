#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), String> {
    linux::run().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the sandbox example runs on Linux only");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
    use orchestrator::block::overlay::Overlay;
    use orchestrator::envd::{ENVD_PORT, EnvdInit};
    use orchestrator::network::settings::NetworkSettings;
    use orchestrator::network::slot::Slot;
    use orchestrator::sandbox::Sandbox;
    use orchestrator::sandbox::config::SandboxBuilder;
    use orchestrator::sandbox::config::sandbox_builder::{
        SetFirecracker, SetId, SetKernel, SetSlot, SetTemplateId, SetWorkDir,
    };
    use orchestrator::storage::backend::Storage;
    use orchestrator::storage::header::{Header, Metadata};
    use orchestrator::tcpfirewall::proxy::{FirewallSandbox, TcpFirewall};
    use orchestrator::template::cache::TemplateCache;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::signal::unix::{Signal, SignalKind, signal};
    use uuid::Uuid;

    type ExampleSandbox = SetWorkDir<SetKernel<SetFirecracker<SetSlot<SetTemplateId<SetId>>>>>;

    const COLD_SLOT: u32 = 4020;
    const RESUMED_SLOT: u32 = 4021;
    const TEARDOWN_POLL: Duration = Duration::from_millis(100);
    const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);

    struct Shutdown {
        terminate: Signal,
    }

    struct Assets {
        firecracker: PathBuf,
        kernel: PathBuf,
        rootfs: PathBuf,
    }

    struct Template {
        dir: tempfile::TempDir,
        storage: Storage,
        store: Arc<DiffStore>,
        cache: TemplateCache,
        header: Header,
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
        let mut shutdown = Shutdown::install()?;
        let assets = assets()?;
        let template = Template::upload(&assets.rootfs).await?;
        let settings = NetworkSettings::current();
        TcpFirewall::new(
            Arc::new(|_| {
                Some(FirewallSandbox {
                    sandbox_id: "local".to_owned(),
                    egress: None,
                })
            }),
            settings,
        )
        .start(settings)
        .await?;
        let listener = TcpListener::bind(("127.0.0.1", ENVD_PORT))
            .await
            .map_err(|e| format!("bind 127.0.0.1:{ENVD_PORT}: {e}"))?;
        let booted = tokio::select! {
            sandbox = boot(&assets, &template) => Some(sandbox?),
            () = shutdown.requested() => None,
        };
        let Some(sandbox) = booted else {
            return wait_for_teardown().await;
        };
        println!("ready {}", sandbox.envd_address());

        let forwarding = tokio::spawn(forward(listener, sandbox.envd_address()));
        shutdown.requested().await;
        forwarding.abort();
        sandbox.stop().await
    }

    async fn boot(assets: &Assets, template: &Template) -> Result<Sandbox, String> {
        match std::env::var("E2B_RESUME").is_ok_and(|value| value == "1") {
            true => resumed(assets, template).await,
            false => cold(assets, template).await,
        }
    }

    async fn wait_for_teardown() -> Result<(), String> {
        let slots = [COLD_SLOT, RESUMED_SLOT].map(Slot::new);
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        while Instant::now() < deadline && slots.iter().flatten().any(Slot::namespace_exists) {
            tokio::time::sleep(TEARDOWN_POLL).await;
        }
        Ok(())
    }

    async fn cold(assets: &Assets, template: &Template) -> Result<Sandbox, String> {
        template
            .sandbox(assets, "sbx-cold", COLD_SLOT)
            .await?
            .rootfs(template.overlay(template.header.clone(), "cold")?)
            .kernel_logs(true)
            .cold_boot()
            .await
    }

    async fn resumed(assets: &Assets, template: &Template) -> Result<Sandbox, String> {
        let source = cold(assets, template).await?;
        let build = source.pause(&template.storage, Uuid::new_v4()).await?.build;
        let started = Instant::now();
        let sandbox = template
            .sandbox(assets, "sbx-resumed", RESUMED_SLOT)
            .await?
            .rootfs(template.overlay(build.rootfs.clone(), "resumed")?)
            .memfile(Arc::new(BuildFile::new(
                build.memfile.clone(),
                Artifact::Memfile,
                template.store.clone(),
            )))
            .resume(&template.snapfile(build.build_id).await?)
            .await?;
        println!("resumed in {} ms", started.elapsed().as_millis());
        Ok(sandbox)
    }

    impl Template {
        async fn upload(rootfs: &Path) -> Result<Self, String> {
            let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
            let storage = Storage::local(&dir.path().join("store"))?;
            let template_id = Uuid::new_v4();
            storage.put_file(&Artifact::Rootfs.object(template_id), rootfs).await?;
            let size = std::fs::metadata(rootfs)
                .map_err(|e| format!("stat {}: {e}", rootfs.display()))?
                .len();
            let header = Header::new(Metadata::template(template_id, 4096, size), Vec::new())?;
            let store = Arc::new(DiffStore::new(storage.clone(), dir.path().join("cache")));
            let cache = TemplateCache::new(storage.clone(), dir.path().join("templates"));
            Ok(Self {
                dir,
                storage,
                store,
                cache,
                header,
            })
        }

        async fn snapfile(&self, build_id: Uuid) -> Result<PathBuf, String> {
            self.cache.get(build_id).snapfile().await.map_err(|e| e.to_string())
        }

        fn overlay(&self, header: Header, name: &str) -> Result<Arc<Overlay>, String> {
            let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, self.store.clone()));
            Overlay::new(base, &self.dir.path().join(format!("{name}.overlay")))
                .map(Arc::new)
                .map_err(|e| format!("create overlay: {e}"))
        }

        async fn sandbox(
            &self,
            assets: &Assets,
            id: &str,
            slot: u32,
        ) -> Result<SandboxBuilder<ExampleSandbox>, String> {
            let slot = Slot::new(slot)?;
            let _ = slot.destroy().await;
            Ok(Sandbox::builder()
                .id(id)
                .template_id(self.header.metadata.build_id.to_string())
                .slot(slot)
                .firecracker(&assets.firecracker)
                .kernel(&assets.kernel)
                .work_dir(self.dir.path().join(id))
                .envd(EnvdInit {
                    default_user: Some("user".to_owned()),
                    default_workdir: Some("/home/user".to_owned()),
                    ..EnvdInit::default()
                }))
        }
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

    impl Shutdown {
        fn install() -> Result<Self, String> {
            signal(SignalKind::terminate())
                .map(|terminate| Self { terminate })
                .map_err(|e| e.to_string())
        }

        async fn requested(&mut self) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = self.terminate.recv() => {}
            }
        }
    }
}
