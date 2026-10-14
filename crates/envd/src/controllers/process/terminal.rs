use std::io;
use std::os::fd::RawFd;
use std::pin::Pin;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

pub type Writer = Pin<Box<dyn AsyncWrite + Send>>;

pub struct Terminal {
    pub fd: RawFd,
    writer: Mutex<Writer>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HandoverFds {
    pub stdout: Option<RawFd>,
    pub stderr: Option<RawFd>,
    pub stdin: Option<RawFd>,
    pub tty: Option<RawFd>,
}

impl Terminal {
    pub fn new(fd: RawFd, writer: Writer) -> Self {
        Self {
            fd,
            writer: Mutex::new(writer),
        }
    }

    pub async fn write_all(&self, data: &[u8]) -> io::Result<()> {
        self.writer.lock().await.write_all(data).await
    }

    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        let size = nix::libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        match unsafe { nix::libc::ioctl(self.fd, nix::libc::TIOCSWINSZ, &size) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}
