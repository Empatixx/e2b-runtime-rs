use std::path::Path;

pub const ROOTFS_MOUNTPOINT: &str = "/";

#[cfg(target_os = "linux")]
mod ioctl {
    nix::ioctl_readwrite!(fifreeze, b'X', 119, nix::libc::c_int);
    nix::ioctl_readwrite!(fithaw, b'X', 120, nix::libc::c_int);
}

#[cfg(target_os = "linux")]
pub fn freeze(mountpoint: &Path) -> Result<(), String> {
    filesystem_ioctl(
        mountpoint,
        "FIFREEZE",
        nix::errno::Errno::EBUSY,
        |fd, argument| unsafe { ioctl::fifreeze(fd, argument) },
    )
}

#[cfg(target_os = "linux")]
pub fn thaw(mountpoint: &Path) -> Result<(), String> {
    filesystem_ioctl(mountpoint, "FITHAW", nix::errno::Errno::EINVAL, |fd, argument| unsafe {
        ioctl::fithaw(fd, argument)
    })
}

#[cfg(target_os = "linux")]
fn filesystem_ioctl(
    mountpoint: &Path,
    name: &str,
    already: nix::errno::Errno,
    call: impl FnOnce(i32, *mut nix::libc::c_int) -> nix::Result<i32>,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let file = std::fs::File::open(mountpoint).map_err(|e| {
        format!(
            "open {}: {}",
            mountpoint.display(),
            crate::util::fs::os_error("open", mountpoint, &e)
        )
    })?;
    let mut argument: nix::libc::c_int = 0;
    match call(file.as_raw_fd(), &mut argument) {
        Ok(_) => Ok(()),
        Err(errno) if errno == already => Ok(()),
        Err(errno) => Err(format!(
            "{name} {}: {}",
            mountpoint.display(),
            crate::util::fs::reason(&std::io::Error::from(errno))
        )),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn freeze(_mountpoint: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn thaw(_mountpoint: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_upstream() {
        assert_eq!(
            nix::request_code_readwrite!(b'X', 119, std::mem::size_of::<nix::libc::c_int>()) as u32,
            0xC004_5877
        );
        assert_eq!(
            nix::request_code_readwrite!(b'X', 120, std::mem::size_of::<nix::libc::c_int>()) as u32,
            0xC004_5878
        );
    }

    #[test]
    fn a_missing_mountpoint_reports_the_open_error() {
        assert_eq!(
            freeze(Path::new("/nonexistent-mountpoint")),
            Err("open /nonexistent-mountpoint: open /nonexistent-mountpoint: no such file or directory".to_owned())
        );
    }
}
