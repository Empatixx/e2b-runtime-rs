use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use nix::libc;

use super::ioctl;
use super::netlink::{Connect, NbdNetlink};
use super::pool::{DEFAULT_POOL_SIZE, DevicePool};

const NOT_OPEN: &str = "no NBD device open to flush";
const IO_TIMEOUT: Duration = Duration::from_secs(90);
const DEAD_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_RETRY: Duration = Duration::from_millis(25);
const STATUS_POLL_INTERVAL: Duration = Duration::from_micros(100);

static POOL: OnceLock<DevicePool> = OnceLock::new();

pub fn host_pool() -> &'static DevicePool {
    POOL.get_or_init(|| DevicePool::new(device_count(), pool_size(), Box::new(is_idle)))
}

struct Claim {
    index: u32,
}

impl Claim {
    async fn take() -> io::Result<Self> {
        host_pool()
            .get()
            .await
            .map(|index| Self { index })
            .map_err(io::Error::other)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        host_pool().release(self.index);
    }
}

pub struct NbdDevice {
    index: u32,
    attached: Option<Attached>,
}

struct Attached {
    control: File,
    _kernel_sockets: Vec<UnixStream>,
    netlink: NbdNetlink,
    claim: Claim,
}

impl Attached {
    async fn finish(self) -> io::Result<()> {
        let disconnected = wait_for_state(&self.netlink, self.claim.index, false, DISCONNECT_TIMEOUT).await;
        drop(self);
        disconnected
    }
}

impl NbdDevice {
    pub async fn attach(kernel_sockets: Vec<UnixStream>, size: u64, block_size: u64) -> io::Result<Self> {
        let netlink = NbdNetlink::open()?;
        let claim = connect(&netlink, &kernel_sockets, size, block_size).await?;
        match open_when_connected(&netlink, claim.index).await {
            Ok(control) => Ok(Self {
                index: claim.index,
                attached: Some(Attached {
                    control,
                    _kernel_sockets: kernel_sockets,
                    netlink,
                    claim,
                }),
            }),
            Err(e) => {
                let _ = netlink.disconnect(claim.index).await;
                let _ = wait_for_state(&netlink, claim.index, false, DISCONNECT_TIMEOUT).await;
                Err(e)
            }
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn path(&self) -> PathBuf {
        device_path(self.index)
    }

    pub async fn flush(&self) -> Result<(), String> {
        let control = self
            .attached
            .as_ref()
            .ok_or_else(|| NOT_OPEN.to_owned())?
            .control
            .try_clone()
            .map_err(|e| format!("sync NBD device: {e}"))?;
        tokio::task::spawn_blocking(move || flush_device(&control))
            .await
            .map_err(|e| format!("flush NBD device: {e}"))?
    }

    pub async fn disconnect(&self) -> io::Result<()> {
        match self.attached.as_ref() {
            Some(attached) => attached.netlink.disconnect(self.index).await,
            None => Ok(()),
        }
    }

    pub async fn wait(mut self) -> io::Result<()> {
        match self.attached.take() {
            Some(attached) => attached.finish().await,
            None => Ok(()),
        }
    }
}

impl Drop for NbdDevice {
    fn drop(&mut self) {
        let Some(attached) = self.attached.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = attached.netlink.disconnect(attached.claim.index).await;
                attached.finish().await
            });
        }
    }
}

async fn connect(netlink: &NbdNetlink, kernel_sockets: &[UnixStream], size: u64, block_size: u64) -> io::Result<Claim> {
    let sockets: Vec<u32> = kernel_sockets.iter().map(|socket| socket.as_raw_fd() as u32).collect();
    loop {
        let claim = Claim::take().await?;
        let request = Connect {
            index: claim.index,
            sockets: &sockets,
            size,
            block_size,
            timeout: IO_TIMEOUT,
            dead_connection_timeout: DEAD_CONNECTION_TIMEOUT,
        };
        match netlink.connect(&request).await {
            Ok(_) => return Ok(claim),
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => return Err(e),
            Err(e) => {
                tracing::error!(error = %e, device_index = claim.index, "error opening NBD, retrying");
                drop(claim);
                tokio::time::sleep(CONNECT_RETRY).await;
            }
        }
    }
}

async fn open_when_connected(netlink: &NbdNetlink, index: u32) -> io::Result<File> {
    wait_for_state(netlink, index, true, CONNECT_TIMEOUT).await?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path(index))
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("error opening NBD device {}: {e}", device_path(index).display()),
            )
        })
}

async fn wait_for_state(netlink: &NbdNetlink, index: u32, connected: bool, timeout: Duration) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if netlink.is_connected(index).await.is_ok_and(|state| state == connected) {
            return Ok(());
        }
        tokio::time::sleep(STATUS_POLL_INTERVAL).await;
    }
    let wanted = if connected { "connect" } else { "disconnect" };
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("nbd{index} did not {wanted} within {timeout:?}"),
    ))
}

fn flush_device(control: &File) -> Result<(), String> {
    let synced = control.sync_all().map_err(|e| format!("sync NBD device: {e}"));
    let invalidated = ioctl::flush_buffers(control).map_err(|e| format!("flush NBD device buffers: {e}"));
    joined_failures([synced.err(), invalidated.err()])
}

fn joined_failures(failures: [Option<String>; 2]) -> Result<(), String> {
    let failures: Vec<String> = failures.into_iter().flatten().collect();
    match failures.is_empty() {
        true => Ok(()),
        false => Err(failures.join("\n")),
    }
}

fn device_count() -> u32 {
    std::fs::read_to_string("/sys/module/nbd/parameters/nbds_max")
        .ok()
        .and_then(|max| max.trim().parse().ok())
        .unwrap_or_else(|| (0..).take_while(|index| device_path(*index).exists()).count() as u32)
}

fn pool_size() -> usize {
    std::env::var("NBD_POOL_SIZE")
        .ok()
        .and_then(|size| size.parse().ok())
        .unwrap_or(DEFAULT_POOL_SIZE)
}

fn is_idle(index: u32) -> bool {
    !pid_path(index).exists()
        && std::fs::read_to_string(sys_path(index).join("size")).is_ok_and(|size| size.trim() == "0")
}

fn device_path(index: u32) -> PathBuf {
    PathBuf::from(format!("/dev/nbd{index}"))
}

fn sys_path(index: u32) -> PathBuf {
    Path::new("/sys/block").join(format!("nbd{index}"))
}

fn pid_path(index: u32) -> PathBuf {
    sys_path(index).join("pid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_failures_join_like_go_errors_join() {
        assert_eq!(joined_failures([None, None]), Ok(()));
        assert_eq!(
            joined_failures([Some("sync NBD device: EIO".to_owned()), None]),
            Err("sync NBD device: EIO".to_owned())
        );
        assert_eq!(
            joined_failures([
                Some("sync NBD device: EIO".to_owned()),
                Some("flush NBD device buffers: EBUSY".to_owned())
            ]),
            Err("sync NBD device: EIO\nflush NBD device buffers: EBUSY".to_owned())
        );
    }
}
