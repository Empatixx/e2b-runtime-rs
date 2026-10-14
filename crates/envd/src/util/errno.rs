use std::fmt;
use std::io;

const TABLE: &[(i32, &str)] = &[
    (1, "operation not permitted"),
    (2, "no such file or directory"),
    (3, "no such process"),
    (4, "interrupted system call"),
    (5, "input/output error"),
    (6, "no such device or address"),
    (7, "argument list too long"),
    (8, "exec format error"),
    (9, "bad file descriptor"),
    (10, "no child processes"),
    (11, "resource temporarily unavailable"),
    (12, "cannot allocate memory"),
    (13, "permission denied"),
    (14, "bad address"),
    (15, "block device required"),
    (16, "device or resource busy"),
    (17, "file exists"),
    (18, "invalid cross-device link"),
    (19, "no such device"),
    (20, "not a directory"),
    (21, "is a directory"),
    (22, "invalid argument"),
    (23, "too many open files in system"),
    (24, "too many open files"),
    (25, "inappropriate ioctl for device"),
    (26, "text file busy"),
    (27, "file too large"),
    (28, "no space left on device"),
    (29, "illegal seek"),
    (30, "read-only file system"),
    (31, "too many links"),
    (32, "broken pipe"),
    (33, "numerical argument out of domain"),
    (34, "numerical result out of range"),
    (35, "resource deadlock avoided"),
    (36, "file name too long"),
    (37, "no locks available"),
    (38, "function not implemented"),
    (39, "directory not empty"),
    (40, "too many levels of symbolic links"),
    (42, "no message of desired type"),
    (43, "identifier removed"),
    (44, "channel number out of range"),
    (45, "level 2 not synchronized"),
    (46, "level 3 halted"),
    (47, "level 3 reset"),
    (48, "link number out of range"),
    (49, "protocol driver not attached"),
    (50, "no CSI structure available"),
    (51, "level 2 halted"),
    (52, "invalid exchange"),
    (53, "invalid request descriptor"),
    (54, "exchange full"),
    (55, "no anode"),
    (56, "invalid request code"),
    (57, "invalid slot"),
    (59, "bad font file format"),
    (60, "device not a stream"),
    (61, "no data available"),
    (62, "timer expired"),
    (63, "out of streams resources"),
    (64, "machine is not on the network"),
    (65, "package not installed"),
    (66, "object is remote"),
    (67, "link has been severed"),
    (68, "advertise error"),
    (69, "srmount error"),
    (70, "communication error on send"),
    (71, "protocol error"),
    (72, "multihop attempted"),
    (73, "RFS specific error"),
    (74, "bad message"),
    (75, "value too large for defined data type"),
    (76, "name not unique on network"),
    (77, "file descriptor in bad state"),
    (78, "remote address changed"),
    (79, "can not access a needed shared library"),
    (80, "accessing a corrupted shared library"),
    (81, ".lib section in a.out corrupted"),
    (82, "attempting to link in too many shared libraries"),
    (83, "cannot exec a shared library directly"),
    (84, "invalid or incomplete multibyte or wide character"),
    (85, "interrupted system call should be restarted"),
    (86, "streams pipe error"),
    (87, "too many users"),
    (88, "socket operation on non-socket"),
    (89, "destination address required"),
    (90, "message too long"),
    (91, "protocol wrong type for socket"),
    (92, "protocol not available"),
    (93, "protocol not supported"),
    (94, "socket type not supported"),
    (95, "operation not supported"),
    (96, "protocol family not supported"),
    (97, "address family not supported by protocol"),
    (98, "address already in use"),
    (99, "cannot assign requested address"),
    (100, "network is down"),
    (101, "network is unreachable"),
    (102, "network dropped connection on reset"),
    (103, "software caused connection abort"),
    (104, "connection reset by peer"),
    (105, "no buffer space available"),
    (106, "transport endpoint is already connected"),
    (107, "transport endpoint is not connected"),
    (108, "cannot send after transport endpoint shutdown"),
    (109, "too many references: cannot splice"),
    (110, "connection timed out"),
    (111, "connection refused"),
    (112, "host is down"),
    (113, "no route to host"),
    (114, "operation already in progress"),
    (115, "operation now in progress"),
    (116, "stale file handle"),
    (117, "structure needs cleaning"),
    (118, "not a XENIX named type file"),
    (119, "no XENIX semaphores available"),
    (120, "is a named type file"),
    (121, "remote I/O error"),
    (122, "disk quota exceeded"),
    (123, "no medium found"),
    (124, "wrong medium type"),
    (125, "operation canceled"),
    (126, "required key not available"),
    (127, "key has expired"),
    (128, "key has been revoked"),
    (129, "key was rejected by service"),
    (130, "owner died"),
    (131, "state not recoverable"),
    (132, "operation not possible due to RF-kill"),
];

#[cfg(target_arch = "aarch64")]
const ARCH_TABLE: &[(i32, &str)] = &[(133, "memory page has hardware error")];

#[cfg(not(target_arch = "aarch64"))]
const ARCH_TABLE: &[(i32, &str)] = &[];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GoErrno {
    code: i32,
}

impl GoErrno {
    pub fn new(code: i32) -> Self {
        Self { code }
    }

    pub fn from_io(err: &io::Error) -> Option<Self> {
        err.raw_os_error().map(Self::new)
    }

    fn text(&self) -> Option<&'static str> {
        TABLE
            .iter()
            .chain(ARCH_TABLE)
            .find(|(code, _)| *code == self.code)
            .map(|(_, text)| *text)
    }
}

impl fmt::Display for GoErrno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.text() {
            Some(text) => f.write_str(text),
            None => write!(f, "errno {}", self.code),
        }
    }
}

pub fn message(err: &io::Error) -> String {
    GoErrno::from_io(err).map_or_else(|| err.to_string(), |errno| errno.to_string())
}

pub fn notify_message(err: &notify::Error) -> String {
    match &err.kind {
        notify::ErrorKind::Io(io_err) => message(io_err),
        notify::ErrorKind::PathNotFound => GoErrno::new(nix::libc::ENOENT).to_string(),
        _ => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn known_codes_use_go_texts() {
        assert_eq!(GoErrno::new(nix::libc::ENOENT).to_string(), "no such file or directory");
        assert_eq!(GoErrno::new(nix::libc::EACCES).to_string(), "permission denied");
        assert_eq!(
            GoErrno::new(nix::libc::EAGAIN).to_string(),
            "resource temporarily unavailable"
        );
        assert_eq!(
            GoErrno::new(nix::libc::ELOOP).to_string(),
            "too many levels of symbolic links"
        );
        assert_eq!(GoErrno::new(nix::libc::ENOTDIR).to_string(), "not a directory");
        assert_eq!(GoErrno::new(nix::libc::ENOSPC).to_string(), "no space left on device");
    }

    #[test]
    fn unknown_codes_print_the_number() {
        assert_eq!(GoErrno::new(0).to_string(), "errno 0");
        assert_eq!(GoErrno::new(9999).to_string(), "errno 9999");
    }

    #[test]
    fn io_errors_with_an_errno_drop_the_os_error_suffix() {
        let err = io::Error::from_raw_os_error(nix::libc::ENOENT);
        assert_eq!(message(&err), "no such file or directory");
    }

    #[test]
    fn notify_errors_use_go_texts() {
        let missing = notify::Error::path_not_found();
        assert_eq!(notify_message(&missing), "no such file or directory");
        let io_err = notify::Error::io(io::Error::from_raw_os_error(nix::libc::EACCES));
        assert_eq!(notify_message(&io_err), "permission denied");
    }

    #[test]
    fn io_errors_without_an_errno_keep_their_text() {
        assert_eq!(message(&io::Error::other("boom")), "boom");
    }
}
