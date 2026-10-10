use std::fmt;

use rustix::io::Errno;

use super::names::kernel_path;
use crate::go_os::reason;

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
            reason(&std::io::Error::from_raw_os_error(self.errno.raw_os_error()))
        )
    }
}

impl std::error::Error for PathError {}

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
