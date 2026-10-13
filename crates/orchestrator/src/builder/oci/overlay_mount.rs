use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use nix::libc;

const FSOPEN_CLOEXEC: libc::c_uint = 1;
const FSCONFIG_SET_STRING: libc::c_uint = 1;
const FSCONFIG_CMD_CREATE: libc::c_uint = 6;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;
const NO_DIRFD: libc::c_int = -1;

pub fn mount_overlay(layers: &[impl AsRef<Path>], mount_point: &Path) -> Result<(), String> {
    let context = FsContext::open("overlay").map_err(|e| format!("fsopen failed: {}", e))?;
    layers.iter().try_for_each(|layer| {
        context
            .set_string("lowerdir+", layer.as_ref())
            .map_err(|e| format!("fsconfig lowerdir failed: {}", e))
    })?;
    context.create().map_err(|e| format!("fsconfig create failed: {}", e))?;
    let mount = context.mount().map_err(|e| format!("fsmount failed: {}", e))?;
    move_mount(&mount, mount_point).map_err(|e| format!("move mount failed: {}", e))
}

struct FsContext {
    fd: OwnedFd,
}

impl FsContext {
    fn open(filesystem: &str) -> io::Result<Self> {
        let name = c_string(filesystem)?;
        let fd = check(unsafe { libc::syscall(libc::SYS_fsopen, name.as_ptr(), FSOPEN_CLOEXEC) })?;
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) },
        })
    }

    fn set_string(&self, key: &str, value: &Path) -> io::Result<()> {
        let key = c_string(key)?;
        let value = c_path(value)?;
        check(unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                self.fd.as_raw_fd(),
                FSCONFIG_SET_STRING,
                key.as_ptr(),
                value.as_ptr(),
                0,
            )
        })
        .map(drop)
    }

    fn create(&self) -> io::Result<()> {
        check(unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                self.fd.as_raw_fd(),
                FSCONFIG_CMD_CREATE,
                std::ptr::null::<libc::c_char>(),
                std::ptr::null::<libc::c_void>(),
                0,
            )
        })
        .map(drop)
    }

    fn mount(&self) -> io::Result<OwnedFd> {
        let fd = check(unsafe { libc::syscall(libc::SYS_fsmount, self.fd.as_raw_fd(), 0, 0) })?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
    }
}

fn move_mount(mount: &OwnedFd, target: &Path) -> io::Result<()> {
    let empty = c_string("")?;
    let target = c_path(target)?;
    check(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            mount.as_raw_fd(),
            empty.as_ptr(),
            NO_DIRFD,
            target.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    })
    .map(drop)
}

fn c_string(text: &str) -> io::Result<CString> {
    CString::new(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn check(result: libc::c_long) -> io::Result<libc::c_long> {
    match result {
        -1 => Err(io::Error::last_os_error()),
        value => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_layers_win_over_lower_ones() {
        if !nix::unistd::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (upper, lower, merged) = (
            dir.path().join("upper"),
            dir.path().join("lower"),
            dir.path().join("merged"),
        );
        [&upper, &lower, &merged]
            .iter()
            .for_each(|path| std::fs::create_dir(path).unwrap());
        std::fs::write(lower.join("shared"), "lower").unwrap();
        std::fs::write(lower.join("only-lower"), "kept").unwrap();
        std::fs::write(upper.join("shared"), "upper").unwrap();

        match mount_overlay(&[&upper, &lower], &merged) {
            Err(e) if e.starts_with("fsconfig lowerdir failed") => return,
            outcome => outcome.unwrap(),
        }
        let shared = std::fs::read_to_string(merged.join("shared")).unwrap();
        let only_lower = std::fs::read_to_string(merged.join("only-lower")).unwrap();
        assert!(
            std::process::Command::new("umount")
                .arg(&merged)
                .status()
                .unwrap()
                .success()
        );

        assert_eq!((shared.as_str(), only_lower.as_str()), ("upper", "kept"));
    }
}
