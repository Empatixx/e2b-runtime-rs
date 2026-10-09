use std::os::fd::RawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use buffa::Message;
use nix::fcntl::{FcntlArg, fcntl};

use super::{DEFAULT_UPGRADE_BIN, HANDOVER_PATH, handover_defaults, schema_for};
use crate::app::AppState;
use crate::cgroups::freeze::{FreezeMode, FreezeOptions};
use crate::controllers::filesystem::handover::export_watchers;
use crate::controllers::process::registry::Exited;
use crate::controllers::process::spawn::Process;
use crate::port::forward;
use crate::proto::upgrade::{HandoverExit, HandoverProc, HandoverState, MountEntry};
use crate::util::fs::reason;

const HANDOVER_MAX_WAIT: Duration = Duration::from_secs(2);
const FD_BASE: RawFd = 200;
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn upgrade(state: &Arc<AppState>, new_bin: Option<&str>) -> String {
    eprintln!(
        "envd: self-upgrade (from v{VERSION}, newBin={:?})",
        new_bin.unwrap_or_default()
    );
    let options = FreezeOptions {
        max_wait: HANDOVER_MAX_WAIT,
        mode: FreezeMode::Legacy,
        max_cgroups: 0,
    };
    let (permit, result, errors) = state.freezer.freeze_hold(options).await;
    let frozen = match (errors.is_empty(), result.all_frozen()) {
        (false, _) => {
            let error = errors.join("\n");
            tracing::warn!(error = %error, "handover: freeze failed");
            Err(error)
        }
        (true, false) => {
            tracing::warn!(frozen = result.frozen, not_frozen = result.not_frozen, failed = result.failed, wait_duration = ?result.wait, "handover: refusing to swap over a workload that did not stop");
            Err("workload was not frozen before the handover".to_owned())
        }
        (true, true) => Ok(()),
    };
    let failure = match frozen {
        Err(e) => format!("handover aborted: freeze workload: {e}"),
        Ok(()) => swap(state, new_bin).await,
    };
    drop(permit);
    if let Err(e) = state.freezer.unfreeze().await {
        tracing::warn!(error = %e, "handover: unfreeze failed");
    }
    failure
}

async fn swap(state: &Arc<AppState>, new_bin: Option<&str>) -> String {
    if let Some(bin) = new_bin.filter(|bin| *bin != DEFAULT_UPGRADE_BIN) {
        return format!("refusing upgrade to unexpected binary {bin:?}");
    }
    let mut running = Vec::new();
    for process in state.processes.all() {
        let stdin_open = process.stdin.lock().await.is_some();
        running.push((process, stdin_open));
    }
    hand_over(state, &running, new_bin)
}

fn hand_over(state: &AppState, running: &[(Arc<Process>, bool)], new_bin: Option<&str>) -> String {
    let watchers = state.watchers.lock();
    let forwards = state.forwards.lock();
    let mut handover = HandoverState {
        from_ver: VERSION.to_owned(),
        watchers: export_watchers(&watchers),
        mounts: state
            .nfs
            .export()
            .into_iter()
            .map(|(path, lifecycle_id)| MountEntry {
                path,
                lifecycle_id,
                ..Default::default()
            })
            .collect(),
        forwards: forward::export(&forwards),
        guest_frozen_cgroups: state.freezer.guest_frozen_paths(),
        defaults: handover_defaults(&state.defaults()).into(),
        terminated: state.processes.retained().iter().map(handover_exit).collect(),
        ..Default::default()
    };
    handover.schema = schema_for(&handover);
    let mut relocated = Vec::new();
    let processes = processes(running, &mut relocated);
    let failure = match processes {
        Err(e) => format!("relocate handover fds: {e}"),
        Ok(processes) => {
            handover.processes = processes;
            exec(&handover, new_bin)
        }
    };
    relocated.into_iter().for_each(|fd| {
        let _ = nix::unistd::close(fd);
    });
    failure
}

fn processes(running: &[(Arc<Process>, bool)], relocated: &mut Vec<RawFd>) -> Result<Vec<HandoverProc>, String> {
    let mut carried = Vec::new();
    for (index, (process, stdin_open)) in running.iter().enumerate() {
        let slot = FD_BASE + index as RawFd * 5;
        let stdin_open = *stdin_open;
        let mut keep = |fd: Option<RawFd>, target: RawFd| -> Result<i32, String> {
            let kept = fd.map(|fd| dup_keep(fd, target)).transpose()?;
            relocated.extend(kept);
            Ok(kept.unwrap_or(-1))
        };
        let fds = &process.handover;
        let stdout_fd = keep(fds.stdout, slot)?;
        let stderr_fd = keep(fds.stderr, slot + 1)?;
        let stdin_fd = keep(fds.stdin.filter(|_| stdin_open), slot + 2)?;
        let tty_fd = keep(fds.tty, slot + 3)?;
        carried.push(HandoverProc {
            pid: process.pid,
            tag: process.tag.clone().unwrap_or_default(),
            has_tag: process.tag.is_some(),
            cg_type: process_type_name(process.process_type).to_owned(),
            config: process.config.clone().into(),
            stdout_fd,
            stderr_fd,
            stdin_fd,
            tty_fd,
            timeout_ms: remaining_ms(*process.deadline.lock()),
            ..Default::default()
        });
    }
    Ok(carried)
}

fn process_type_name(process_type: crate::cgroups::ProcessType) -> &'static str {
    crate::cgroups::process_type_name(process_type)
}

fn remaining_ms(deadline: Option<Instant>) -> i64 {
    deadline.map_or(0, |deadline| {
        let remaining = deadline.saturating_duration_since(Instant::now()).as_millis() as i64;
        remaining.max(1)
    })
}

fn handover_exit(exited: &Exited) -> HandoverExit {
    HandoverExit {
        pid: exited.pid,
        tag: exited.tag.clone().unwrap_or_default(),
        has_tag: exited.tag.is_some(),
        end: exited.end.clone().into(),
        remaining_ms: exited.expires.saturating_duration_since(Instant::now()).as_millis() as i64,
        ..Default::default()
    }
}

fn dup_keep(fd: RawFd, target: RawFd) -> Result<RawFd, String> {
    if fcntl(target, FcntlArg::F_GETFD).is_ok() {
        return Err(format!("handover fd target {target} already in use"));
    }
    nix::unistd::dup2(fd, target).map_err(|e| format!("dup3 {fd}->{target}: {}", reason(&e.into())))
}

fn exec(handover: &HandoverState, new_bin: Option<&str>) -> String {
    let blob = handover.encode_to_vec();
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(HANDOVER_PATH)
        .and_then(|mut file| std::io::Write::write_all(&mut file, &blob));
    if let Err(e) = written {
        return format!("write handover: {}", crate::util::errno::message(&e));
    }
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(e) => return format!("resolve executable: {}", crate::util::errno::message(&e)),
    };
    let bin = new_bin.map_or(executable, std::path::PathBuf::from);
    let error = std::process::Command::new(&bin)
        .args(std::env::args().skip(1))
        .arg("--resume-handover")
        .exec();
    format!("execve {}: {}", bin.display(), reason(&error))
}
