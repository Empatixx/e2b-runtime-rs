use std::fmt;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use tokio::process::Command;
use tokio::time::Instant;

use super::super::jail::jail_properties;
use super::super::{is_nbd_device, read_capped};
use super::{Debugfs, ENVD_SWAP_TIMEOUT};
use crate::fs::path_error;

const DEBUGFS: &str = "/usr/sbin/debugfs";
const MAX_OUTPUT: u64 = 64 << 10;
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SCRIPT_MODE: u32 = 0o644;

pub struct JailedDebugfs {
    pub device: String,
    pub stage: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugfsRunError {
    pub phase: String,
    pub exit_code: i32,
    pub output_len: usize,
    pub cause: Option<String>,
}

impl fmt::Display for DebugfsRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (phase, len) = (&self.phase, self.output_len);
        match (self.exit_code, &self.cause) {
            (code, _) if code >= 0 => write!(f, "debugfs {phase} exited {code} ({len} bytes output withheld)"),
            (_, Some(cause)) => write!(
                f,
                "debugfs {phase} did not complete: {cause} ({len} bytes output withheld)"
            ),
            (_, None) => write!(f, "debugfs {phase} did not complete ({len} bytes output withheld)"),
        }
    }
}

impl DebugfsRunError {
    pub fn from_status(phase: &str, status: std::io::Result<ExitStatus>, output_len: usize) -> Option<Self> {
        let failed = |exit_code: i32, cause: Option<String>| {
            Some(Self {
                phase: phase.to_owned(),
                exit_code,
                output_len,
                cause,
            })
        };
        match status {
            Ok(status) if status.success() => None,
            Ok(status) => match status.code() {
                Some(code) => failed(code, None),
                None => failed(-1, Some(status.to_string())),
            },
            Err(e) => failed(-1, Some(e.to_string())),
        }
    }
}

impl Debugfs for JailedDebugfs {
    fn stage_dir(&self) -> &Path {
        &self.stage
    }

    async fn run(&self, phase: &str, script: &str, writable: bool, deadline: Instant) -> Result<Vec<u8>, String> {
        if !is_nbd_device(&self.device) {
            return Err(format!(
                "refusing to run debugfs on unexpected device path {:?}",
                self.device
            ));
        }
        let script_path = self.write_script(phase, script)?;
        let unit = format!("envd-swap-{phase}-{}", device_name(&self.device));
        let stage = self.stage.to_string_lossy();
        let ran = self.spawn(&unit, &stage, &script_path, writable, deadline).await;
        stop_unit(&unit).await;
        let (output, status) = ran;
        match DebugfsRunError::from_status(phase, status, output.len()) {
            None => Ok(output),
            Some(error) => Err(error.to_string()),
        }
    }
}

impl JailedDebugfs {
    fn write_script(&self, phase: &str, script: &str) -> Result<PathBuf, String> {
        let path = self.stage.join(format!("cmds-{phase}"));
        std::fs::write(&path, script)
            .map_err(|e| format!("write debugfs script: {}", path_error("open", &path, &e)))?;
        std::fs::set_permissions(&path, Permissions::from_mode(SCRIPT_MODE))
            .map_err(|e| format!("chmod debugfs script: {}", path_error("chmod", &path, &e)))?;
        Ok(path)
    }

    async fn spawn(
        &self,
        unit: &str,
        stage: &str,
        script: &Path,
        writable: bool,
        deadline: Instant,
    ) -> (Vec<u8>, std::io::Result<ExitStatus>) {
        let write_flag = writable.then_some("-w");
        let spawned = Command::new("systemd-run")
            .args(jail_properties(unit, ENVD_SWAP_TIMEOUT, &self.device, &[stage]))
            .args(["--", DEBUGFS])
            .args(write_flag)
            .arg("-f")
            .arg(script)
            .arg(&self.device)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => return (Vec::new(), Err(e)),
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let output = tokio::spawn(async move {
            let (out, err) = tokio::join!(
                async {
                    match stdout {
                        Some(stdout) => read_capped(stdout, MAX_OUTPUT).await,
                        None => Vec::new(),
                    }
                },
                async {
                    match stderr {
                        Some(stderr) => read_capped(stderr, MAX_OUTPUT).await,
                        None => Vec::new(),
                    }
                }
            );
            let mut output = [out, err].concat();
            output.truncate(MAX_OUTPUT as usize);
            output
        });
        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                let _ = child.start_kill();
                child.wait().await
            }
        };
        (output.await.unwrap_or_default(), status)
    }
}

fn device_name(device: &str) -> &str {
    device.rsplit('/').next().unwrap_or(device)
}

async fn stop_unit(unit: &str) {
    let stop = Command::new("systemctl")
        .args(["stop", "--quiet", &format!("{unit}.service")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    let _ = tokio::time::timeout(STOP_TIMEOUT, stop).await;
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    #[test]
    fn run_errors_carry_only_structural_facts() {
        let error = |exit_code: i32, cause: Option<&str>| DebugfsRunError {
            phase: "swap".to_owned(),
            exit_code,
            output_len: 42,
            cause: cause.map(str::to_owned),
        };

        assert_eq!(
            error(1, None).to_string(),
            "debugfs swap exited 1 (42 bytes output withheld)"
        );
        assert_eq!(
            error(-1, Some("signal: killed")).to_string(),
            "debugfs swap did not complete: signal: killed (42 bytes output withheld)"
        );
        assert_eq!(
            error(-1, None).to_string(),
            "debugfs swap did not complete (42 bytes output withheld)"
        );
    }

    #[test]
    fn a_status_maps_to_the_exit_code_or_the_signal() {
        let exited = DebugfsRunError::from_status("backup", Ok(ExitStatus::from_raw(2 << 8)), 7).unwrap();
        assert_eq!((exited.exit_code, exited.cause), (2, None));

        let killed = DebugfsRunError::from_status("backup", Ok(ExitStatus::from_raw(9)), 0).unwrap();
        assert_eq!(
            (killed.exit_code, killed.cause.as_deref()),
            (-1, Some("signal: 9 (SIGKILL)"))
        );

        assert!(DebugfsRunError::from_status("backup", Ok(ExitStatus::from_raw(0)), 0).is_none());
    }

    #[tokio::test]
    async fn a_non_nbd_device_is_refused_before_anything_runs() {
        let stage = tempfile::tempdir().unwrap();
        let debugfs = JailedDebugfs {
            device: "/dev/sda".to_owned(),
            stage: stage.path().to_path_buf(),
        };

        let refused = debugfs.run("stat", "stat /usr/bin/envd\n", false, Instant::now()).await;

        assert_eq!(
            refused,
            Err("refusing to run debugfs on unexpected device path \"/dev/sda\"".to_owned())
        );
        assert!(!stage.path().join("cmds-stat").exists());
    }
}
