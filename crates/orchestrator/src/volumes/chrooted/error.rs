use std::fmt;

use rustix::io::Errno;

use super::names::kernel_path;

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct PathError {
    #[builder(into)]
    pub operation: String,
    #[builder(into)]
    pub path: String,
    pub errno: Errno,
}

impl PathError {
    pub fn is_not_found(&self) -> bool {
        self.errno == Errno::NOENT
    }

    pub fn is_exist(&self) -> bool {
        self.errno == Errno::EXIST || self.errno == Errno::NOTEMPTY
    }

    pub fn has(&self, errno: Errno) -> bool {
        self.errno == errno
    }
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            self.operation,
            self.path,
            errno_text(self.errno)
        )
    }
}

impl std::error::Error for PathError {}

pub fn errno_text(errno: Errno) -> String {
    os_error_text(&std::io::Error::from_raw_os_error(errno.raw_os_error()))
}

pub fn os_error_text(err: &std::io::Error) -> String {
    err.raw_os_error()
        .map(|code| client_errno_text(code).unwrap_or_else(|| nix::errno::Errno::from_raw(code).desc()))
        .map(|desc| {
            let first = desc.chars().take(1).flat_map(char::to_lowercase);
            first.chain(desc.chars().skip(1)).collect()
        })
        .unwrap_or_else(|| err.to_string())
}

fn client_errno_text(code: i32) -> Option<&'static str> {
    match code {
        nix::libc::EOPNOTSUPP => Some("operation not supported"),
        nix::libc::EXDEV => Some("invalid cross-device link"),
        nix::libc::EIO => Some("input/output error"),
        nix::libc::ESTALE => Some("stale NFS file handle"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct LinkError {
    #[builder(into)]
    pub operation: String,
    #[builder(into)]
    pub old: String,
    #[builder(into)]
    pub new: String,
    pub errno: Errno,
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} {}: {}",
            self.operation,
            self.old,
            self.new,
            errno_text(self.errno)
        )
    }
}

impl std::error::Error for LinkError {}

pub fn link_failed(operation: &'static str, old: String, new: String) -> impl Fn(Errno) -> LinkError {
    move |errno| {
        LinkError::builder()
            .operation(operation)
            .old(old.clone())
            .new(new.clone())
            .errno(errno)
            .build()
    }
}

pub fn failed<'a>(operation: &'a str, name: &'a str) -> impl Fn(Errno) -> PathError + 'a {
    move |errno| {
        PathError::builder()
            .operation(operation)
            .path(kernel_path(name))
            .errno(errno)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_like_a_go_path_error() {
        let error = failed("mkdir", "a/b")(Errno::EXIST);

        assert_eq!(error.to_string(), "mkdir /a/b: file exists");
        assert!(error.is_exist());
    }

    #[test]
    fn a_non_empty_directory_counts_as_existing_like_go() {
        assert!(failed("rename", "/a")(Errno::NOTEMPTY).is_exist());
        assert!(!failed("rename", "/a")(Errno::NOENT).is_exist());
        assert!(failed("rename", "/a")(Errno::NOENT).is_not_found());
    }
}
