use std::io;
use std::path::{Path, PathBuf};

pub fn path_error(op: &str, path: &Path, err: &io::Error) -> String {
    format!("{op} {}: {}", path.display(), reason(err))
}

pub fn parent(path: &Path) -> PathBuf {
    path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

pub fn reason(err: &io::Error) -> String {
    err.raw_os_error()
        .map(|code| go_errno_text(code).unwrap_or_else(|| nix::errno::Errno::from_raw(code).desc()))
        .map(|desc| {
            let first = desc.chars().take(1).flat_map(char::to_lowercase);
            first.chain(desc.chars().skip(1)).collect()
        })
        .unwrap_or_else(|| err.to_string())
}

fn go_errno_text(code: i32) -> Option<&'static str> {
    match code {
        nix::libc::EOPNOTSUPP => Some("operation not supported"),
        nix::libc::EXDEV => Some("invalid cross-device link"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_like_go_path_errors() {
        let error = io::Error::from_raw_os_error(nix::libc::EACCES);

        assert_eq!(
            path_error("mkdir", Path::new("/x"), &error),
            "mkdir /x: permission denied"
        );
    }
}
