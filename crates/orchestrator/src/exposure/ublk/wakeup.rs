use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use io_uring::{opcode, types};
use libublk::io::UblkQueue;
use nix::libc;
use nix::sys::eventfd::{EfdFlags, EventFd};

#[derive(Clone)]
pub struct Wakeup {
    eventfd: Arc<EventFd>,
}

pub struct Signal {
    eventfd: Arc<EventFd>,
}

impl Wakeup {
    pub fn new() -> io::Result<Self> {
        EventFd::from_flags(EfdFlags::EFD_CLOEXEC | EfdFlags::EFD_NONBLOCK)
            .map(|eventfd| Self {
                eventfd: Arc::new(eventfd),
            })
            .map_err(io::Error::from)
    }

    pub fn signal(&self) -> Signal {
        Signal {
            eventfd: self.eventfd.clone(),
        }
    }

    pub async fn listen(&self, queue: &UblkQueue<'_>) {
        let readable = opcode::PollAdd::new(types::Fd(self.eventfd.as_raw_fd()), libc::POLLIN as u32).build();
        while queue.ublk_submit_sqe(readable.clone()).await >= 0 {
            let _ = self.eventfd.read();
        }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        let _ = self.eventfd.write(1);
    }
}
