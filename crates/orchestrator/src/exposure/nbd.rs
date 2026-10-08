pub mod pool;
pub mod protocol;
pub mod server;

#[cfg(target_os = "linux")]
pub mod device;
#[cfg(target_os = "linux")]
mod ioctl;

#[cfg(target_os = "linux")]
pub use linux::{Exposure, expose};

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    use super::device::NbdDevice;
    use super::server::serve;
    use crate::block::overlay::Overlay;
    use crate::storage::header::PAGE_SIZE;

    pub struct Exposure {
        device: NbdDevice,
        server: JoinHandle<Result<(), String>>,
        serving: watch::Receiver<()>,
    }

    pub async fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
        let (kernel_socket, server_socket) = std::os::unix::net::UnixStream::pair()?;
        server_socket.set_nonblocking(true)?;
        let size = overlay.size();
        let socket = tokio::net::UnixStream::from_std(server_socket)?;
        let (alive, serving) = watch::channel(());
        let server = tokio::spawn(async move {
            let served = serve(overlay, socket).await;
            drop(alive);
            served
        });
        match NbdDevice::attach(kernel_socket, size, PAGE_SIZE).await {
            Ok(device) => Ok(Exposure {
                device,
                server,
                serving,
            }),
            Err(e) => {
                server.abort();
                Err(e)
            }
        }
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
                .request_disconnect()
                .map_err(|e| format!("disconnect nbd device: {e}"));
            if disconnected.is_err() {
                self.server.abort();
            }
            let served = self
                .server
                .await
                .map_err(|e| format!("nbd server task: {e}"))
                .and_then(|served| served);
            if let Err(e) = self.device.wait().await {
                tracing::debug!(error = %e, "nbd worker ended with an error after disconnect");
            }
            disconnected.and(served)
        }
    }
}
