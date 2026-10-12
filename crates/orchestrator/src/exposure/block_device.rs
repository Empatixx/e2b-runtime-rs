use std::fs::File;
use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;

use crate::block::overlay::Overlay;

mod raw {
    nix::ioctl_none_bad!(flush_buffers, nix::request_code_none!(0x12, 97));
}

pub trait BlockDevice: Send + Sync + 'static {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> impl Future<Output = Result<(), String>> + Send;
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String>;
    fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String>;
}

impl BlockDevice for Overlay {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        Overlay::read_at(self, offset, buf).await
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String> {
        Overlay::write_at(self, offset, data)
    }

    fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String> {
        Overlay::write_zeroes_at(self, offset, length)
    }
}

pub fn flush(device: &File, name: &str) -> Result<(), String> {
    let synced = device.sync_all().map_err(|e| format!("sync {name} device: {e}"));
    let invalidated = flush_buffers(device).map_err(|e| format!("flush {name} device buffers: {e}"));
    joined_failures([synced.err(), invalidated.err()])
}

fn flush_buffers(device: &File) -> io::Result<()> {
    unsafe { raw::flush_buffers(device.as_raw_fd()) }
        .map(drop)
        .map_err(io::Error::from)
}

fn joined_failures(failures: [Option<String>; 2]) -> Result<(), String> {
    let failures: Vec<String> = failures.into_iter().flatten().collect();
    match failures.is_empty() {
        true => Ok(()),
        false => Err(failures.join("\n")),
    }
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

    #[test]
    fn flush_names_the_device_in_its_errors() {
        let file = tempfile::tempfile().unwrap();
        assert_eq!(
            flush(&file, "ublk"),
            Err("flush ublk device buffers: Inappropriate ioctl for device (os error 25)".to_owned())
        );
    }
}
