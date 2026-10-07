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

    pub fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
        let (kernel_socket, server_socket) = std::os::unix::net::UnixStream::pair()?;
        server_socket.set_nonblocking(true)?;
        let size = overlay.size();
        let server = tokio::spawn(serve(overlay, tokio::net::UnixStream::from_std(server_socket)?));
        let device = NbdDevice::attach(kernel_socket, size, PAGE_SIZE)?;
        Ok(Exposure { device, server })
    }

    impl Exposure {
        pub fn path(&self) -> PathBuf {
            self.device.path()
        }

        pub async fn close(self) -> Result<(), String> {
            self.device
                .disconnect()
                .map_err(|e| format!("disconnect nbd device: {e}"))?;
            self.server.await.map_err(|e| format!("nbd server task: {e}"))?
        }
    }
}
