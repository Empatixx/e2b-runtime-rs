use std::io;
use std::path::Path;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpgid};

use crate::go_os::{path_error, reason};

const FIRECRACKER_BINARY: &str = "firecracker";

pub fn reclaim(proc_dir: &Path) -> (u64, Vec<String>) {
    let pids = match discover(proc_dir) {
        Ok(pids) => pids,
        Err(error) => return (0, vec![error]),
    };
    pids.into_iter()
        .fold((0, Vec::new()), |(reclaimed, mut failures), pid| {
            match kill_group(pid) {
                Ok(()) => (reclaimed + 1, failures),
                Err(error) => {
                    failures.push(error);
                    (reclaimed, failures)
                }
            }
        })
}

fn kill_group(pid: i32) -> Result<(), String> {
    let group = getpgid(Some(Pid::from_raw(pid)))
        .map_err(|e| format!("failed to get firecracker pgid for pid {pid}: {}", go_errno(e)))?;
    match killpg(group, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {
            tracing::warn!(pid, pgid = group.as_raw(), "killed orphaned firecracker process group");
            Ok(())
        }
        Err(e) => Err(format!(
            "failed to kill firecracker process group {} for pid {pid}: {}",
            group.as_raw(),
            go_errno(e)
        )),
    }
}

fn go_errno(errno: Errno) -> String {
    reason(&io::Error::from(errno))
}

pub fn discover(proc_dir: &Path) -> Result<Vec<i32>, String> {
    let entries = std::fs::read_dir(proc_dir)
        .map_err(|e| format!("failed to read proc directory: {}", path_error("open", proc_dir, &e)))?;
    let mut pids: Vec<i32> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| is_firecracker(proc_dir, *pid))
        .collect();
    pids.sort_unstable();
    Ok(pids)
}

fn is_firecracker(proc_dir: &Path, pid: i32) -> bool {
    std::fs::read(proc_dir.join(pid.to_string()).join("cmdline"))
        .ok()
        .and_then(|cmdline| {
            let program = cmdline.split(|byte| *byte == 0).next()?.to_vec();
            String::from_utf8(program).ok()
        })
        .is_some_and(|program| {
            Path::new(&program)
                .file_name()
                .is_some_and(|name| name == FIRECRACKER_BINARY)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(proc_dir: &Path, pid: &str, cmdline: &[u8]) {
        std::fs::create_dir_all(proc_dir.join(pid)).unwrap();
        std::fs::write(proc_dir.join(pid).join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn only_firecracker_binaries_are_found_like_go() {
        let dir = tempfile::tempdir().unwrap();
        process(
            dir.path(),
            "10",
            b"/fc-versions/v1.12/firecracker\0--api-sock\0/tmp/fc.sock\0",
        );
        process(dir.path(), "11", b"firecracker\0");
        process(dir.path(), "12", b"/usr/bin/firecracker-monitor\0");
        process(dir.path(), "13", b"");
        process(dir.path(), "self", b"firecracker\0");
        std::fs::write(dir.path().join("14"), b"").unwrap();

        assert_eq!(discover(dir.path()).unwrap(), vec![10, 11]);
    }

    #[test]
    fn an_unreadable_proc_dir_is_one_failure() {
        let (reclaimed, failures) = reclaim(Path::new("/definitely/not/proc"));
        assert_eq!(reclaimed, 0);
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures,
            vec!["failed to read proc directory: open /definitely/not/proc: no such file or directory".to_owned()]
        );
    }
}
