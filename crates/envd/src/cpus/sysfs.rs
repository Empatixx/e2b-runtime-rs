use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;

use super::mask::{parse_cpu, parse_mask};
use crate::reaper::Registry;
use crate::reexec::{Helper, SELF_EXECUTABLE};
use crate::util::errno;
use crate::util::fs::os_error;

pub const SYSFS_CPU_DIR: &str = "/sys/devices/system/cpu";
const MAX_MASK_BYTES: u64 = 64 << 10;
const WRITE_USAGE: &str = "usage: cpu-online PATH 0|1";

pub const WRITE_ONLINE: Helper = Helper {
    name: "cpu-online",
    run: write_online_file,
};

pub trait CpuSysfs: Send + Sync + 'static {
    fn possible(&self) -> Result<Vec<usize>, String>;
    fn online(&self) -> Result<Vec<usize>, String>;
    fn set_online(&self, cpu: usize, online: bool) -> impl Future<Output = Result<(), String>> + Send;
}

#[derive(bon::Builder)]
pub struct KernelSysfs {
    #[builder(default = PathBuf::from(SYSFS_CPU_DIR), into)]
    dir: PathBuf,
    #[builder(default = PathBuf::from(SELF_EXECUTABLE), into)]
    executable: PathBuf,
    children: Arc<Registry>,
}

impl CpuSysfs for KernelSysfs {
    fn possible(&self) -> Result<Vec<usize>, String> {
        self.read_mask("possible")
    }

    fn online(&self) -> Result<Vec<usize>, String> {
        self.read_mask("online")
    }

    async fn set_online(&self, cpu: usize, online: bool) -> Result<(), String> {
        let attribute = self.dir.join(format!("cpu{cpu}")).join("online");
        let value = if online { "1" } else { "0" };
        let mut command = WRITE_ONLINE.command(
            &self.executable,
            &[attribute.to_string_lossy().into_owned(), value.to_owned()],
        );
        command.stdin(Stdio::null()).stderr(Stdio::piped());
        let output = self.children.run(command).await.map_err(|e| errno::message(&e))?;
        child_outcome(&output)
    }
}

impl KernelSysfs {
    fn read_mask(&self, name: &str) -> Result<Vec<usize>, String> {
        let path = self.dir.join(name);
        let file = fs::File::open(&path).map_err(|e| os_error("open", &path, &e))?;
        let mut mask = String::new();
        file.take(MAX_MASK_BYTES + 1)
            .read_to_string(&mut mask)
            .map_err(|e| os_error("read", &path, &e))?;
        if mask.len() as u64 > MAX_MASK_BYTES {
            return Err(format!("{name}: longer than {MAX_MASK_BYTES} bytes"));
        }
        parse_mask(mask.trim())
    }
}

fn child_outcome(output: &Output) -> Result<(), String> {
    let failure = match output.status.code() {
        Some(0) => return Ok(()),
        _ => describe(output.status),
    };
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(match message.is_empty() {
        true => failure,
        false => format!("{failure}: {message}"),
    })
}

fn describe(status: ExitStatus) -> String {
    status.code().map_or_else(
        || format!("signal: {}", status.signal().unwrap_or_default()),
        |code| format!("exit status {code}"),
    )
}

pub fn write_online_file(args: &[String]) -> Result<(), String> {
    let [attribute, value] = args else {
        return Err(WRITE_USAGE.to_owned());
    };
    if !matches!(value.as_str(), "0" | "1") {
        return Err(WRITE_USAGE.to_owned());
    }
    let path = Path::new(attribute);
    if !is_online_attribute(path) {
        let shown: String = attribute.chars().take(64).collect();
        return Err(format!("{shown:?} is not a cpuN/online file"));
    }
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| os_error("open", path, &e))?
        .write_all(value.as_bytes())
        .map_err(|e| os_error("write", path, &e))
}

fn is_online_attribute(path: &Path) -> bool {
    let lexical = path
        .components()
        .all(|component| !matches!(component, Component::ParentDir | Component::CurDir));
    let is_cpu_directory = path
        .parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .and_then(|directory| directory.strip_prefix("cpu"))
        .is_some_and(|number| parse_cpu(number).is_ok());
    lexical && is_cpu_directory && path.file_name() == Some(OsStr::new("online"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn write_online_rejects_bad_args_naming_the_failed_check() {
        let usage = "usage:";
        let not_cpu = "is not a cpuN/online file";
        for (name, args, expected) in [
            ("missing value", vec!["/sys/devices/system/cpu/cpu2/online"], usage),
            ("not a bit", vec!["/sys/devices/system/cpu/cpu2/online", "2"], usage),
            (
                "not a cpu dir",
                vec!["/sys/devices/system/cpu/cpuX/online", "1"],
                not_cpu,
            ),
            (
                "negative cpu",
                vec!["/sys/devices/system/cpu/cpu-2/online", "1"],
                not_cpu,
            ),
            ("huge cpu", vec!["/sys/devices/system/cpu/cpu8192/online", "1"], not_cpu),
            ("not online", vec!["/sys/devices/system/cpu/cpu2/uevent", "1"], not_cpu),
            ("other file", vec!["/etc/passwd", "1"], not_cpu),
            (
                "traversal",
                vec!["/sys/devices/system/cpu/cpu2/online/../../../../../etc/passwd", "1"],
                not_cpu,
            ),
        ] {
            let error = write_online_file(&text(&args)).unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn write_online_never_creates_the_attribute() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("cpu3")).unwrap();
        let attribute = root.path().join("cpu3").join("online");

        let error = write_online_file(&text(&[attribute.to_str().unwrap(), "1"])).unwrap_err();

        assert!(error.ends_with("cpu3/online: no such file or directory"), "{error}");
        assert!(!attribute.exists());
    }

    #[test]
    fn write_online_replaces_the_attribute_content() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("cpu2")).unwrap();
        let attribute = root.path().join("cpu2").join("online");
        fs::write(&attribute, "0\n").unwrap();

        write_online_file(&text(&[attribute.to_str().unwrap(), "1"])).unwrap();

        assert_eq!(fs::read_to_string(&attribute).unwrap(), "1");
    }

    #[test]
    fn the_masks_are_read_and_parsed() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("possible"), "0-3\n").unwrap();
        fs::write(root.path().join("online"), "0-1\n").unwrap();
        let sysfs = KernelSysfs::builder().dir(root.path()).children(Arc::default()).build();

        assert_eq!(sysfs.possible(), Ok(vec![0, 1, 2, 3]));
        assert_eq!(sysfs.online(), Ok(vec![0, 1]));
    }

    #[test]
    fn an_oversized_mask_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let oversized = "0".repeat(MAX_MASK_BYTES as usize + 1);
        fs::write(root.path().join("online"), oversized).unwrap();
        let sysfs = KernelSysfs::builder().dir(root.path()).children(Arc::default()).build();

        assert!(sysfs.online().unwrap_err().contains("longer than"));
    }

    #[test]
    fn child_failures_carry_the_status_and_the_helpers_message() {
        let output = |code: i32, stderr: &str| Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        assert_eq!(child_outcome(&output(0, "")), Ok(()));
        assert_eq!(child_outcome(&output(1, "")), Err("exit status 1".to_owned()));
        assert_eq!(
            child_outcome(&output(1, "open x: busy\n")),
            Err("exit status 1: open x: busy".to_owned())
        );
    }
}
