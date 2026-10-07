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

    use tokio::task::JoinHandle;

    use super::device::NbdDevice;
    use super::server::serve;
    use crate::block::overlay::Overlay;
    use crate::storage::header::PAGE_SIZE;

    pub struct Exposure {
        device: NbdDevice,
        server: JoinHandle<Result<(), String>>,
    }

    pub async fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
        let (kernel_socket, server_socket) = std::os::unix::net::UnixStream::pair()?;
        server_socket.set_nonblocking(true)?;
        let size = overlay.size();
        let server = tokio::spawn(serve(overlay, tokio::net::UnixStream::from_std(server_socket)?));
        match NbdDevice::attach(kernel_socket, size, PAGE_SIZE).await {
            Ok(device) => Ok(Exposure { device, server }),
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

        pub async fn close(self) -> Result<(), String> {
            if let Err(e) = self.device.request_disconnect() {
                self.server.abort();
                let _ = self.device.wait().await;
                return Err(format!("disconnect nbd device: {e}"));
            }
            let served = self.server.await.map_err(|e| format!("nbd server task: {e}"))?;
            if let Err(e) = self.device.wait().await {
                tracing::debug!(error = %e, "nbd worker ended with an error after disconnect");
            }
            served
        }
    }
}
