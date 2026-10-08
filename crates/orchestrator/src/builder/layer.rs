use std::path::Path;

use super::logs::ScopedLog;
use super::sandbox_command::{CommandLog, ROOT, run_logged, run_quiet};
use crate::envd::files::copy_file;
use crate::envd::process::CommandContext;
use crate::go_exec::GoCommand;
use crate::proto::LogLevel;
use crate::sandbox::Sandbox;

const TMP_ENVD_PATH: &str = "/tmp/envd_updated";
const GUEST_ENVD_PATH: &str = "/usr/bin/envd";
const REPLACE_LOG_ID: &str = "update-envd-replace";
const RESTART_ENVD: &str = "if command -v systemctl >/dev/null 2>&1; then
    systemctl restart envd
else
    kill -TERM \"$(pidof envd)\"
fi";

pub async fn envd_version(envd: &Path) -> Result<String, String> {
    GoCommand::new(envd)
        .args(["-version"])
        .run()
        .await
        .map(|output| String::from_utf8_lossy(&output).trim().to_owned())
        .map_err(|e| format!("error while getting envd version: {}", e.message))
}

pub async fn update_envd(log: &ScopedLog<'_>, sandbox: &mut Sandbox, host_envd: &Path) -> Result<(), String> {
    let version = envd_version(host_envd)
        .await
        .map_err(|e| format!("error getting envd version: {e}"))?;
    log.push(LogLevel::Debug, format!("Updating envd to version v{version}"));
    let envd = sandbox.envd_address();
    copy_file(envd, ROOT, host_envd, TMP_ENVD_PATH)
        .await
        .map_err(|e| format!("failed to copy envd binary to sandbox: {e}"))?;
    let root = CommandContext {
        user: ROOT.to_owned(),
        ..CommandContext::default()
    };
    let output = CommandLog {
        log,
        level: LogLevel::Debug,
        id: REPLACE_LOG_ID,
    };
    run_logged(envd, &replace_script(), &root, &output)
        .await
        .map_err(|e| format!("failed to replace envd binary: {e}"))?;
    let _ = run_quiet(envd, RESTART_ENVD, &root).await;
    sandbox
        .wait_for_envd()
        .await
        .map_err(|e| format!("failed to wait for envd initialization after update: {e}"))
}

fn replace_script() -> String {
    format!(
        "\n\t\t# Replace the binary and set permissions\n\t\tchmod +x {TMP_ENVD_PATH}\n\t\tmv -f {TMP_ENVD_PATH} {GUEST_ENVD_PATH}\n\t"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_replace_script_matches_upstream_bytes() {
        assert_eq!(
            replace_script(),
            "\n\t\t# Replace the binary and set permissions\n\t\tchmod +x /tmp/envd_updated\n\t\tmv -f /tmp/envd_updated /usr/bin/envd\n\t"
        );
    }
}
