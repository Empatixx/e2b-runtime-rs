use std::io;
use std::path::{Path, PathBuf};

pub fn path_error(op: &str, path: &Path, err: &io::Error) -> String {
    format!("{op} {}: {}", path.display(), reason(err))
}

pub fn parent(path: &Path) -> PathBuf {
    path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn reason(err: &io::Error) -> String {
    err.raw_os_error()
        .map(|code| nix::errno::Errno::from_raw(code).desc())
        .map(|desc| {
            let first = desc.chars().take(1).flat_map(char::to_lowercase);
            first.chain(desc.chars().skip(1)).collect()
        })
        .unwrap_or_else(|| err.to_string())
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
