use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

mod raw {
    use nix::{ioctl_none_bad, ioctl_write_int_bad, request_code_none};

    ioctl_write_int_bad!(set_sock, request_code_none!(0xab, 0));
    ioctl_write_int_bad!(set_block_size, request_code_none!(0xab, 1));
    ioctl_none_bad!(do_it, request_code_none!(0xab, 3));
    ioctl_none_bad!(clear_sock, request_code_none!(0xab, 4));
    ioctl_write_int_bad!(set_size_blocks, request_code_none!(0xab, 7));
    ioctl_none_bad!(disconnect, request_code_none!(0xab, 8));
    ioctl_write_int_bad!(set_timeout, request_code_none!(0xab, 9));
    ioctl_write_int_bad!(set_flags, request_code_none!(0xab, 10));
}

pub fn set_sock(device: &File, socket: RawFd) -> io::Result<()> {
    check(unsafe { raw::set_sock(device.as_raw_fd(), socket) })
}

pub fn set_block_size(device: &File, block_size: u64) -> io::Result<()> {
    check(unsafe { raw::set_block_size(device.as_raw_fd(), block_size as i32) })
}

pub fn set_size_blocks(device: &File, blocks: u64) -> io::Result<()> {
    check(unsafe { raw::set_size_blocks(device.as_raw_fd(), blocks as i32) })
}

pub fn set_timeout(device: &File, seconds: u64) -> io::Result<()> {
    check(unsafe { raw::set_timeout(device.as_raw_fd(), seconds as i32) })
}

pub fn set_flags(device: &File, flags: i32) -> io::Result<()> {
    check(unsafe { raw::set_flags(device.as_raw_fd(), flags) })
}

pub fn do_it(device: &File) -> io::Result<()> {
    check(unsafe { raw::do_it(device.as_raw_fd()) })
}

pub fn clear_sock(device: &File) -> io::Result<()> {
    check(unsafe { raw::clear_sock(device.as_raw_fd()) })
}

pub fn disconnect(device: &File) -> io::Result<()> {
    check(unsafe { raw::disconnect(device.as_raw_fd()) })
}

fn check(result: nix::Result<i32>) -> io::Result<()> {
    result.map(|_| ()).map_err(io::Error::from)
}
