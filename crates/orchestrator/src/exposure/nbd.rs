pub mod pool;
pub mod protocol;
pub mod server;

#[cfg(target_os = "linux")]
pub mod device;
#[cfg(target_os = "linux")]
mod ioctl;
#[cfg(target_os = "linux")]
mod netlink;

#[cfg(target_os = "linux")]
pub use linux::{Exposure, expose};

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::Arc;

    use futures::future::join_all;
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    use super::device::NbdDevice;
    use super::server::serve;
    use crate::block::overlay::Overlay;
    use crate::storage::header::PAGE_SIZE;

    const CONNECTIONS_PER_DEVICE: usize = 1;

    pub struct Exposure {
        device: NbdDevice,
        servers: Vec<JoinHandle<Result<(), String>>>,
        serving: watch::Receiver<()>,
    }

    pub async fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
        let size = overlay.size();
        let (alive, serving) = watch::channel(());
        let alive = Arc::new(alive);
        let mut kernel_sockets = Vec::with_capacity(CONNECTIONS_PER_DEVICE);
        let mut servers = Vec::with_capacity(CONNECTIONS_PER_DEVICE);
        for _ in 0..CONNECTIONS_PER_DEVICE {
            let (kernel_socket, server_socket) = UnixStream::pair()?;
            kernel_sockets.push(kernel_socket);
            servers.push(spawn_server(overlay.clone(), server_socket, alive.clone())?);
        }
        match NbdDevice::attach(kernel_sockets, size, PAGE_SIZE).await {
            Ok(device) => Ok(Exposure {
                device,
                servers,
                serving,
            }),
            Err(e) => {
                servers.iter().for_each(JoinHandle::abort);
                Err(e)
            }
        }
    }

    fn spawn_server(
        overlay: Arc<Overlay>,
        server_socket: UnixStream,
        alive: Arc<watch::Sender<()>>,
    ) -> io::Result<JoinHandle<Result<(), String>>> {
        server_socket.set_nonblocking(true)?;
        let socket = tokio::net::UnixStream::from_std(server_socket)?;
        Ok(tokio::spawn(async move {
            let served = serve(overlay, socket).await;
            drop(alive);
            served
        }))
    }

    impl Exposure {
        pub fn path(&self) -> PathBuf {
            self.device.path()
        }

        pub async fn flush(&self) -> Result<(), String> {
            self.device.flush().await
        }

        pub async fn ended(&self) {
            let mut serving = self.serving.clone();
            while serving.changed().await.is_ok() {}
        }

        pub async fn close(self) -> Result<(), String> {
            let disconnected = self
                .device
                .disconnect()
                .await
                .map_err(|e| format!("error disconnecting NBD: {e}"));
            if disconnected.is_err() {
                self.servers.iter().for_each(JoinHandle::abort);
            }
            let served = join_all(self.servers)
                .await
                .into_iter()
                .map(|server| {
                    server
                        .map_err(|e| format!("nbd server task: {e}"))
                        .and_then(|served| served)
                })
                .collect::<Result<Vec<()>, String>>();
            if let Err(e) = self.device.wait().await {
                tracing::debug!(error = %e, "nbd device did not report the disconnect");
            }
            disconnected.and(served.map(drop))
        }
    }
}
