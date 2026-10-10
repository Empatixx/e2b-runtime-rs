use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use buffa::Message;
use nix::fcntl::{FcntlArg, OFlag, fcntl};

use super::{HANDOVER_PATH, HandoverReport, MAX_SCHEMA, apply_defaults};
use crate::app::AppState;
use crate::cgroups::ProcessType;
use crate::controllers::filesystem::handover::import_watchers;
use crate::controllers::process::adopt::{Adoption, readopt};
use crate::controllers::process::registry::Exited;
use crate::proto::upgrade::{HandoverExit, HandoverProc, HandoverState};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn resume(state: &Arc<AppState>) -> Result<HandoverReport, String> {
    let resumed = resume_frozen(state).await;
    if !matches!(resumed, Ok(Some(_)))
        && let Err(e) = state.freezer.unfreeze().await
    {
        tracing::warn!(error = %e, "handover: unfreeze failed");
    }
    resumed.map(Option::unwrap_or_default)
}

async fn resume_frozen(state: &Arc<AppState>) -> Result<Option<HandoverReport>, String> {
    let blob = match std::fs::read(HANDOVER_PATH) {
        Ok(blob) => blob,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read handover: {}", crate::util::errno::message(&e))),
    };
    let handover = HandoverState::decode_from_slice(&blob).map_err(|e| format!("unmarshal handover: {e}"))?;
    if handover.schema > MAX_SCHEMA {
        return Err(format!(
            "handover schema {} exceeds max supported {MAX_SCHEMA}",
            handover.schema
        ));
    }
    let (user, workdir) = apply_defaults(handover.defaults.as_option(), &mut state.defaults.write());
    if user || workdir {
        eprintln!("envd: restored exec context from the handover blob (user={user} workdir={workdir})");
    }
    eprintln!(
        "envd: resumed as v{VERSION} after handover (from v{}, {} procs)",
        handover.from_ver,
        handover.processes.len()
    );
    handover.processes.iter().for_each(|carried| adopt(state, carried));
    state
        .processes
        .restore(handover.terminated.iter().map(retained).collect());
    let _ = std::fs::remove_file(HANDOVER_PATH);
    let (rearmed, failed) = import_watchers(&state.watchers, &handover.watchers);
    tracing::info!(
        event_type = "handover_resumed",
        from_ver = %handover.from_ver,
        procs = handover.processes.len(),
        procs_failed = 0,
        retained = handover.terminated.len(),
        retained_failed = 0,
        watchers = rearmed + failed,
        watchers_failed = failed,
        "re-adopted workload after envd self-upgrade"
    );
    state.freezer.set_guest_frozen_paths(&handover.guest_frozen_cgroups);
    if let Err(e) = state.freezer.resume_frozen().await {
        tracing::error!(error = %e, "inherited a frozen workload without the spawn barrier: only the pre-spawn probe guards process starts until /init");
    }
    state.nfs.import(
        handover
            .mounts
            .iter()
            .map(|mount| (mount.path.clone(), mount.lifecycle_id.clone())),
    );
    handover.helper_pids.iter().for_each(|pid| state.children.adopt(*pid));
    let readopted = state.forwards.import(&handover.forwards);
    if readopted > 0 {
        eprintln!("envd: re-adopted {readopted} forwarded port(s) after handover");
    }
    Ok(Some(HandoverReport {
        failed: false,
        procs: handover.processes.len(),
        procs_failed: 0,
        retained: handover.terminated.len(),
        retained_failed: 0,
        watchers: rearmed + failed,
        watchers_failed: failed,
    }))
}

fn adopt(state: &Arc<AppState>, carried: &HandoverProc) {
    let pid = carried.pid;
    let adoption = Adoption {
        pid,
        tag: carried.has_tag.then(|| carried.tag.clone()),
        config: carried.config.as_option().cloned().unwrap_or_default(),
        process_type: process_type(&carried.cg_type),
        stdout: relocated(carried.stdout_fd),
        stderr: relocated(carried.stderr_fd),
        stdin: relocated(carried.stdin_fd),
        tty: relocated(carried.tty_fd),
        timeout: (carried.timeout_ms > 0).then(|| Duration::from_millis(carried.timeout_ms as u64)),
    };
    readopt(adoption, state.processes.clone(), state.freezer.thawed());
    tracing::info!(
        event_type = "process_readopted",
        pid,
        "re-adopted process after envd self-upgrade"
    );
}

fn relocated(fd: i32) -> Option<OwnedFd> {
    if fd < 0 {
        return None;
    }
    let fresh = match fcntl(fd, FcntlArg::F_DUPFD_CLOEXEC(0)) {
        Ok(fresh) => {
            let _ = nix::unistd::close(fd);
            fresh
        }
        Err(e) => {
            tracing::warn!(error = %e, fd, "handover: relocate carried fd off fdBase failed");
            fd as RawFd
        }
    };
    blocking(fresh);
    Some(unsafe { OwnedFd::from_raw_fd(fresh) })
}

fn blocking(fd: RawFd) {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map(OFlag::from_bits_truncate);
    if let Ok(flags) = flags {
        let _ = fcntl(fd, FcntlArg::F_SETFL(flags - OFlag::O_NONBLOCK));
    }
}

fn process_type(name: &str) -> ProcessType {
    match name {
        "pty" => ProcessType::Pty,
        "socat" => ProcessType::Socat,
        "system" => ProcessType::System,
        _ => ProcessType::User,
    }
}

fn retained(carried: &HandoverExit) -> Exited {
    Exited {
        pid: carried.pid,
        tag: carried.has_tag.then(|| carried.tag.clone()),
        end: carried.end.as_option().cloned().unwrap_or_default(),
        expires: Instant::now() + Duration::from_millis(carried.remaining_ms.max(0) as u64),
    }
}
