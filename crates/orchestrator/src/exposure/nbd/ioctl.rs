use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use nix::errno::Errno;
use nix::libc::{self, c_ulong};

const _: () = assert!(size_of::<c_ulong>() == size_of::<u64>());

mod raw {
    use nix::{ioctl_none_bad, ioctl_write_int_bad, request_code_none};

    ioctl_write_int_bad!(set_sock, request_code_none!(0xab, 0));
    ioctl_none_bad!(do_it, request_code_none!(0xab, 3));
    ioctl_none_bad!(clear_sock, request_code_none!(0xab, 4));
    ioctl_none_bad!(disconnect, request_code_none!(0xab, 8));
    ioctl_write_int_bad!(set_flags, request_code_none!(0xab, 10));
}

const SET_BLOCK_SIZE: libc::Ioctl = nix::request_code_none!(0xab, 1) as libc::Ioctl;
const SET_SIZE_BLOCKS: libc::Ioctl = nix::request_code_none!(0xab, 7) as libc::Ioctl;
const SET_TIMEOUT: libc::Ioctl = nix::request_code_none!(0xab, 9) as libc::Ioctl;

pub fn set_sock(device: &File, socket: RawFd) -> io::Result<()> {
    check(unsafe { raw::set_sock(device.as_raw_fd(), socket) })
}

pub fn set_block_size(device: &File, block_size: u64) -> io::Result<()> {
    write_word(device, SET_BLOCK_SIZE, block_size)
}

pub fn set_size_blocks(device: &File, blocks: u64) -> io::Result<()> {
    write_word(device, SET_SIZE_BLOCKS, blocks)
}

pub fn set_timeout(device: &File, seconds: u64) -> io::Result<()> {
    write_word(device, SET_TIMEOUT, seconds)
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

fn write_word(device: &File, request: libc::Ioctl, word: u64) -> io::Result<()> {
    check(Errno::result(unsafe {
        libc::ioctl(device.as_raw_fd(), request, word as c_ulong)
    }))
}

fn check(result: nix::Result<i32>) -> io::Result<()> {
    result.map(|_| ()).map_err(io::Error::from)
}
