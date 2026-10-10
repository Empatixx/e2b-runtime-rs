use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

mod raw {
    use nix::{ioctl_none_bad, request_code_none};

    ioctl_none_bad!(flush_buffers, request_code_none!(0x12, 97));
}

pub fn flush_buffers(device: &File) -> io::Result<()> {
    unsafe { raw::flush_buffers(device.as_raw_fd()) }
        .map(|_| ())
        .map_err(io::Error::from)
}
