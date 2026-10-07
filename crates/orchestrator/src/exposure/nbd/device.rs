use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use super::ioctl;

const FLAGS: i32 = 1 | 4 | 32 | 64;
const TIMEOUT_SECONDS: u64 = 60;
const CONNECT_POLLS: u32 = 200;

pub struct NbdDevice {
    index: u32,
    control: File,
    _kernel_socket: UnixStream,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl NbdDevice {
    pub fn attach(kernel_socket: UnixStream, size: u64, block_size: u64) -> io::Result<Self> {
        let index = free_index()?;
        let control = OpenOptions::new().read(true).write(true).open(device_path(index))?;
        configure(&control, &kernel_socket, size, block_size)?;
        let worker = spawn_worker(control.try_clone()?);
        wait_until_connected(index)?;
        Ok(Self {
            index,
            control,
            _kernel_socket: kernel_socket,
            worker: Some(worker),
        })
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn path(&self) -> PathBuf {
        device_path(self.index)
    }

    pub fn disconnect(mut self) -> io::Result<()> {
        ioctl::disconnect(&self.control)?;
        let served = self
            .worker
            .take()
            .map(|worker| worker.join().map_err(|_| io::Error::other("nbd worker panicked")))
            .transpose()?;
        if let Some(Err(e)) = served {
            tracing::debug!(index = self.index, error = %e, "nbd worker ended with an error");
        }
        Ok(())
    }
}

fn configure(control: &File, kernel_socket: &UnixStream, size: u64, block_size: u64) -> io::Result<()> {
    ioctl::set_block_size(control, block_size)?;
    ioctl::set_size_blocks(control, size / block_size)?;
    ioctl::set_timeout(control, TIMEOUT_SECONDS)?;
    ioctl::set_flags(control, FLAGS)?;
    ioctl::set_sock(control, kernel_socket.as_raw_fd())
}

fn spawn_worker(control: File) -> JoinHandle<io::Result<()>> {
    std::thread::spawn(move || {
        let served = ioctl::do_it(&control);
        let _ = ioctl::clear_sock(&control);
        served
    })
}

fn wait_until_connected(index: u32) -> io::Result<()> {
    (0..CONNECT_POLLS)
        .find(|_| {
            let connected = pid_path(index).exists();
            if !connected {
                std::thread::sleep(Duration::from_millis(10));
            }
            connected
        })
        .map(|_| ())
        .ok_or_else(|| io::Error::other(format!("nbd{index} did not connect")))
}

fn free_index() -> io::Result<u32> {
    (0..)
        .take_while(|index| device_path(*index).exists())
        .find(|index| !pid_path(*index).exists())
        .ok_or_else(|| io::Error::other("no free nbd device (load the nbd module with a larger nbds_max)"))
}

fn device_path(index: u32) -> PathBuf {
    PathBuf::from(format!("/dev/nbd{index}"))
}

fn pid_path(index: u32) -> PathBuf {
    Path::new("/sys/block").join(format!("nbd{index}")).join("pid")
}
