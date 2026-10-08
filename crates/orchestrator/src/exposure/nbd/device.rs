use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::Duration;

use super::ioctl;
use super::pool::{DEFAULT_POOL_SIZE, DevicePool};

const FLAGS: i32 = 1 | 32 | 64 | 256;
const TIMEOUT_SECONDS: u64 = 90;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_micros(100);

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
    kernel_socket: UnixStream,
    worker: JoinHandle<io::Result<()>>,
    claim: Claim,
}

impl Attached {
    fn finish(self) -> io::Result<()> {
        let Self {
            control,
            kernel_socket,
            worker,
            claim,
        } = self;
        let served = worker.join().map_err(|_| io::Error::other("nbd worker panicked"));
        drop(kernel_socket);
        drop(control);
        drop(claim);
        served?
    }
}

impl NbdDevice {
    pub async fn attach(kernel_socket: UnixStream, size: u64, block_size: u64) -> io::Result<Self> {
        let claim = Claim::take().await?;
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .open(device_path(claim.index))?;
        if let Err(e) = configure(&control, &kernel_socket, size, block_size) {
            reset(&control);
            return Err(e);
        }
        let worker = match control.try_clone() {
            Ok(worker_control) => spawn_worker(worker_control),
            Err(e) => {
                reset(&control);
                return Err(e);
            }
        };
        let device = Self {
            index: claim.index,
            attached: Some(Attached {
                control,
                kernel_socket,
                worker,
                claim,
            }),
        };
        match device.wait_until_connected(size).await {
            Ok(()) => Ok(device),
            Err(waited) => Err(device.abandon().await.err().unwrap_or(waited)),
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn path(&self) -> PathBuf {
        device_path(self.index)
    }

    pub fn request_disconnect(&self) -> io::Result<()> {
        self.attached
            .as_ref()
            .map_or(Ok(()), |attached| ioctl::disconnect(&attached.control))
    }

    pub async fn wait(mut self) -> io::Result<()> {
        let Some(attached) = self.attached.take() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || attached.finish())
            .await
            .map_err(io::Error::other)?
    }

    fn is_finished(&self) -> bool {
        self.attached
            .as_ref()
            .is_none_or(|attached| attached.worker.is_finished())
    }

    async fn wait_until_connected(&self, size: u64) -> io::Result<()> {
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if pid_path(self.index()).exists() && self.reported_size() == Some(size) {
                return Ok(());
            }
            if self.is_finished() {
                return Err(io::Error::other(format!(
                    "nbd{} stopped before connecting",
                    self.index()
                )));
            }
            tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
        }
        Err(io::Error::other(format!(
            "nbd{} did not connect with {size} bytes (reports {:?})",
            self.index(),
            self.reported_size()
        )))
    }

    fn reported_size(&self) -> Option<u64> {
        std::fs::read_to_string(sys_path(self.index()).join("size"))
            .ok()
            .and_then(|sectors| sectors.trim().parse::<u64>().ok())
            .map(|sectors| sectors * 512)
    }

    async fn abandon(self) -> io::Result<()> {
        if let Some(attached) = self.attached.as_ref() {
            let _ = ioctl::disconnect(&attached.control);
            reset(&attached.control);
        }
        self.wait().await
    }
}

impl Drop for NbdDevice {
    fn drop(&mut self) {
        if let Some(attached) = self.attached.take() {
            let _ = ioctl::disconnect(&attached.control);
            std::thread::spawn(move || drop(attached.finish()));
        }
    }
}

fn configure(control: &File, kernel_socket: &UnixStream, size: u64, block_size: u64) -> io::Result<()> {
    ioctl::set_block_size(control, block_size)?;
    ioctl::set_size_blocks(control, size / block_size)?;
    ioctl::set_timeout(control, TIMEOUT_SECONDS)?;
    ioctl::set_flags(control, FLAGS)?;
    ioctl::set_sock(control, kernel_socket.as_raw_fd())
}

fn reset(control: &File) {
    let _ = ioctl::clear_sock(control);
    let _ = ioctl::set_size_blocks(control, 0);
}

fn spawn_worker(control: File) -> JoinHandle<io::Result<()>> {
    std::thread::spawn(move || {
        let served = ioctl::do_it(&control);
        let _ = ioctl::clear_sock(&control);
        served
    })
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
