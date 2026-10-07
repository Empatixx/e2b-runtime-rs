use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;

use super::ioctl;

const FLAGS: i32 = 1 | 4 | 32 | 64;
const TIMEOUT_SECONDS: u64 = 90;
const CONNECT_POLLS: u32 = 200;
const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(10);

static CLAIMED: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

struct Claim {
    index: u32,
}

impl Claim {
    fn next_free() -> io::Result<Self> {
        let mut claimed = CLAIMED.lock();
        let index = (0..)
            .take_while(|index| device_path(*index).exists())
            .find(|index| !claimed.contains(index) && is_idle(*index))
            .ok_or_else(|| io::Error::other("no free nbd device (load the nbd module with a larger nbds_max)"))?;
        claimed.insert(index);
        Ok(Self { index })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        CLAIMED.lock().remove(&self.index);
    }
}

pub struct NbdDevice {
    claim: Claim,
    control: File,
    _kernel_socket: UnixStream,
    worker: JoinHandle<io::Result<()>>,
}

impl NbdDevice {
    pub async fn attach(kernel_socket: UnixStream, size: u64, block_size: u64) -> io::Result<Self> {
        let claim = Claim::next_free()?;
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .open(device_path(claim.index))?;
        if let Err(e) = configure(&control, &kernel_socket, size, block_size) {
            reset(&control);
            return Err(e);
        }
        let worker = spawn_worker(control.try_clone()?);
        let device = Self {
            claim,
            control,
            _kernel_socket: kernel_socket,
            worker,
        };
        match device.wait_until_connected().await {
            Ok(()) => Ok(device),
            Err(waited) => Err(device.abandon().await.err().unwrap_or(waited)),
        }
    }

    pub fn index(&self) -> u32 {
        self.claim.index
    }

    pub fn path(&self) -> PathBuf {
        device_path(self.claim.index)
    }

    pub fn request_disconnect(&self) -> io::Result<()> {
        ioctl::disconnect(&self.control)
    }

    pub async fn wait(self) -> io::Result<()> {
        let Self {
            claim,
            control,
            _kernel_socket: kernel_socket,
            worker,
        } = self;
        let served = tokio::task::spawn_blocking(move || worker.join())
            .await
            .map_err(io::Error::other)?
            .map_err(|_| io::Error::other("nbd worker panicked"))?;
        drop(kernel_socket);
        drop(control);
        drop(claim);
        served
    }

    async fn wait_until_connected(&self) -> io::Result<()> {
        for _ in 0..CONNECT_POLLS {
            if pid_path(self.index()).exists() {
                return Ok(());
            }
            if self.worker.is_finished() {
                return Err(io::Error::other(format!(
                    "nbd{} stopped before connecting",
                    self.index()
                )));
            }
            tokio::time::sleep(CONNECT_POLL_INTERVAL).await;
        }
        Err(io::Error::other(format!("nbd{} did not connect", self.index())))
    }

    async fn abandon(self) -> io::Result<()> {
        let _ = ioctl::disconnect(&self.control);
        reset(&self.control);
        self.wait().await
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
