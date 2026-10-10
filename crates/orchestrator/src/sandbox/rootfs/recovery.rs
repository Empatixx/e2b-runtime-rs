use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::process::Command;
use uuid::Uuid;

use super::jail::jail_properties;
use super::{is_nbd_device, read_capped};
use crate::go_exec::status_text;

pub const FS_RECOVER_TIMEOUT: Duration = Duration::from_secs(15);
const E2FSCK: &str = "/usr/sbin/e2fsck";
const RECOVERY_FAILED: &str = "rootfs filesystem recovery did not complete";
const DEVICE_REFUSED: &str = "refusing to run e2fsck on unexpected device path";
const RC_PREFIX: &str = "__E2FSCK_RC__";
const RC_SUFFIX: &str = "__";
const REPLAYED_MAX: i32 = 4;
const SENTINEL_LIMIT: u64 = 4 << 10;
const WAIT_DELAY: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_DEVICE: &str = "/nonexistent-fs-recover-probe";

const SUPERBLOCK_OFFSET: u64 = 1024;
const SUPERBLOCK_SIZE: usize = 1024;
const MAGIC_OFFSET: usize = 0x38;
const EXT4_MAGIC: u16 = 0xEF53;
const INCOMPAT_OFFSET: usize = 0x60;
const INCOMPAT_RECOVER: u32 = 0x0004;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecoverOutcome {
    Replayed,
    Failed,
    FailedOpen,
    SkippedQuiesced,
    #[default]
    None,
}

impl RecoverOutcome {
    pub fn label(self) -> &'static str {
        match self {
            RecoverOutcome::Replayed => "replayed",
            RecoverOutcome::Failed => "failed_operational",
            RecoverOutcome::FailedOpen => "failed_open",
            RecoverOutcome::SkippedQuiesced => "skipped_quiesced",
            RecoverOutcome::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverReason {
    NothingToDo,
    JournalReplayed,
    ReplayUnknown,
    Timeout,
    Killed,
    NoSentinel,
    E2fsck4,
    E2fsck8,
    E2fsckOther,
    LauncherFailure,
    ExecFailure,
    Quiesced,
}

impl RecoverReason {
    pub fn label(self) -> &'static str {
        match self {
            RecoverReason::NothingToDo => "nothing_to_do",
            RecoverReason::JournalReplayed => "journal_replayed",
            RecoverReason::ReplayUnknown => "replay_unknown",
            RecoverReason::Timeout => "timeout",
            RecoverReason::Killed => "killed",
            RecoverReason::NoSentinel => "no_sentinel",
            RecoverReason::E2fsck4 => "e2fsck_4",
            RecoverReason::E2fsck8 => "e2fsck_8",
            RecoverReason::E2fsckOther => "e2fsck_other",
            RecoverReason::LauncherFailure => "launcher_failure",
            RecoverReason::ExecFailure => "exec_failure",
            RecoverReason::Quiesced => "quiesced",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    pub outcome: RecoverOutcome,
    pub reason: RecoverReason,
    pub error: Option<String>,
}

impl Recovery {
    fn booted(outcome: RecoverOutcome, reason: RecoverReason) -> Self {
        Self {
            outcome,
            reason,
            error: None,
        }
    }

    fn failed(reason: RecoverReason, detail: &str) -> Self {
        Self {
            outcome: RecoverOutcome::Failed,
            reason,
            error: Some(format!("{RECOVERY_FAILED}: {detail}")),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryRecord(Arc<Mutex<RecoverOutcome>>);

impl RecoveryRecord {
    pub fn set(&self, outcome: RecoverOutcome) {
        *self.0.lock() = outcome;
    }

    pub fn get(&self) -> RecoverOutcome {
        *self.0.lock()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalState {
    Unknown,
    Clean,
    Dirty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunError {
    None,
    Refused(String),
    Exit { code: i32, text: String },
    Other(String),
}

impl RunError {
    fn text(&self) -> Option<&str> {
        match self {
            RunError::None => None,
            RunError::Refused(text) | RunError::Exit { text, .. } | RunError::Other(text) => Some(text),
        }
    }
}

struct Run {
    sentinel: Vec<u8>,
    error: RunError,
    timed_out: bool,
}

pub async fn recover_filesystem(device: &str) -> Recovery {
    let journal = match is_nbd_device(device) {
        true => journal_state(device).await,
        false => JournalState::Unknown,
    };
    let run = run_e2fsck(device).await;
    classify(&run.error, &run.sentinel, run.timed_out, journal)
}

pub async fn journal_only_supported() -> bool {
    let probed = Command::new(E2FSCK)
        .args(["-n", "-E", "journal_only", PROBE_DEVICE])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .await;
    !probed.is_ok_and(|output| journal_only_rejected(&output.stderr))
}

fn journal_only_rejected(stderr: &[u8]) -> bool {
    stderr
        .windows(b"extended option".len())
        .any(|window| window == b"extended option")
}

async fn journal_state(device: &str) -> JournalState {
    let device = device.to_owned();
    let probe = tokio::task::spawn_blocking(move || read_journal_state(Path::new(&device)));
    tokio::time::timeout(PROBE_TIMEOUT, probe)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(JournalState::Unknown)
}

fn read_journal_state(device: &Path) -> JournalState {
    let mut superblock = [0u8; SUPERBLOCK_SIZE];
    File::open(device)
        .and_then(|file| file.read_exact_at(&mut superblock, SUPERBLOCK_OFFSET))
        .map_or(JournalState::Unknown, |()| parse_journal_state(&superblock))
}

fn parse_journal_state(superblock: &[u8]) -> JournalState {
    let magic = superblock
        .get(MAGIC_OFFSET..MAGIC_OFFSET + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
    let incompat = superblock
        .get(INCOMPAT_OFFSET..INCOMPAT_OFFSET + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
    match (superblock.len() >= SUPERBLOCK_SIZE, magic, incompat) {
        (true, Some(EXT4_MAGIC), Some(incompat)) if incompat & INCOMPAT_RECOVER != 0 => JournalState::Dirty,
        (true, Some(EXT4_MAGIC), Some(_)) => JournalState::Clean,
        _ => JournalState::Unknown,
    }
}

fn e2fsck_rc(sentinel: &[u8]) -> Option<i32> {
    let text = String::from_utf8_lossy(sentinel);
    text.split(RC_PREFIX)
        .skip(1)
        .filter_map(|rest| rest.split_once(RC_SUFFIX))
        .map(|(digits, _)| digits)
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .filter_map(|digits| digits.parse().ok())
        .last()
}

fn classify(error: &RunError, sentinel: &[u8], timed_out: bool, journal: JournalState) -> Recovery {
    match e2fsck_rc(sentinel) {
        Some(rc) => classify_exit(rc, journal),
        None => classify_missing(error, timed_out),
    }
}

fn classify_exit(rc: i32, journal: JournalState) -> Recovery {
    match rc {
        0 => Recovery::booted(RecoverOutcome::Replayed, replay_reason(journal)),
        rc if rc < REPLAYED_MAX => Recovery::booted(RecoverOutcome::Replayed, RecoverReason::JournalReplayed),
        126 | 127 => Recovery::booted(RecoverOutcome::FailedOpen, RecoverReason::ExecFailure),
        rc => Recovery::failed(failed_reason(rc), &format!("e2fsck exit {rc}")),
    }
}

fn replay_reason(journal: JournalState) -> RecoverReason {
    match journal {
        JournalState::Clean => RecoverReason::NothingToDo,
        JournalState::Dirty => RecoverReason::JournalReplayed,
        JournalState::Unknown => RecoverReason::ReplayUnknown,
    }
}

fn failed_reason(rc: i32) -> RecoverReason {
    match rc {
        4 => RecoverReason::E2fsck4,
        8 => RecoverReason::E2fsck8,
        _ => RecoverReason::E2fsckOther,
    }
}

fn classify_missing(error: &RunError, timed_out: bool) -> Recovery {
    match error {
        RunError::Refused(refused) => Recovery::failed(RecoverReason::NoSentinel, refused),
        _ if timed_out => Recovery::failed(
            RecoverReason::Timeout,
            &format!(
                "e2fsck did not report a result: {}",
                error.text().unwrap_or("recovery timed out with no result")
            ),
        ),
        RunError::Exit { code, text } if *code < 0 || *code >= 128 => {
            Recovery::failed(RecoverReason::Killed, &format!("recovery unit signalled: {text}"))
        }
        RunError::Exit { code, .. } if *code > 0 => {
            Recovery::booted(RecoverOutcome::FailedOpen, RecoverReason::LauncherFailure)
        }
        RunError::None | RunError::Exit { .. } => {
            Recovery::failed(RecoverReason::NoSentinel, "e2fsck did not report a result")
        }
        RunError::Other(other) => Recovery::failed(
            RecoverReason::NoSentinel,
            &format!("e2fsck did not report a result: {other}"),
        ),
    }
}

async fn run_e2fsck(device: &str) -> Run {
    if !is_nbd_device(device) {
        return Run {
            sentinel: Vec::new(),
            error: RunError::Refused(format!("{DEVICE_REFUSED} {}", crate::go_strconv::quote(device))),
            timed_out: false,
        };
    }
    let unit = format!("fs-recover-{}", Uuid::new_v4());
    let run = run_jailed(&unit, device).await;
    if run.error != RunError::None {
        stop_unit(&unit).await;
    }
    run
}

async fn run_jailed(unit: &str, device: &str) -> Run {
    let script = format!(r#"{E2FSCK} -p -E journal_only "$0" 1>&2; printf '{RC_PREFIX}%d{RC_SUFFIX}' "$?""#);
    let mut child = match Command::new("systemd-run")
        .args(jail_properties(unit, FS_RECOVER_TIMEOUT, device, &[]))
        .args(["--", "/bin/sh", "-c", &script, device])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return Run {
                sentinel: Vec::new(),
                error: RunError::Other(e.to_string()),
                timed_out: false,
            };
        }
    };
    let stdout = child.stdout.take();
    let sentinel = tokio::spawn(async move {
        match stdout {
            Some(stdout) => read_capped(stdout, SENTINEL_LIMIT).await,
            None => Vec::new(),
        }
    });
    let (status, timed_out) = match tokio::time::timeout(FS_RECOVER_TIMEOUT, child.wait()).await {
        Ok(status) => (status, false),
        Err(_) => {
            let _ = child.start_kill();
            let status = tokio::time::timeout(WAIT_DELAY, child.wait())
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("wait delay expired")));
            (status, true)
        }
    };
    let sentinel = tokio::time::timeout(WAIT_DELAY, sentinel)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    Run {
        sentinel,
        error: run_error(status),
        timed_out,
    }
}

fn run_error(status: std::io::Result<std::process::ExitStatus>) -> RunError {
    match status {
        Ok(status) if status.success() => RunError::None,
        Ok(status) => RunError::Exit {
            code: status.code().unwrap_or(-1),
            text: status_text(status),
        },
        Err(e) => RunError::Other(e.to_string()),
    }
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
    use std::time::Instant;

    use super::*;

    fn sentinel(rc: i32) -> Vec<u8> {
        format!("{RC_PREFIX}{rc}{RC_SUFFIX}").into_bytes()
    }

    fn exit(code: i32) -> RunError {
        RunError::Exit {
            code,
            text: format!("exit status {code}"),
        }
    }

    fn outcome_and_reason(recovery: &Recovery) -> (RecoverOutcome, RecoverReason) {
        (recovery.outcome, recovery.reason)
    }

    #[test]
    fn e2fsck_results_classify_like_go() {
        use RecoverOutcome::{Failed, FailedOpen, Replayed};
        use RecoverReason::*;
        let cases = [
            (
                "replay clean",
                sentinel(0),
                RunError::None,
                false,
                Replayed,
                NothingToDo,
            ),
            (
                "replay corrected",
                sentinel(1),
                RunError::None,
                false,
                Replayed,
                JournalReplayed,
            ),
            (
                "replay reboot",
                sentinel(2),
                RunError::None,
                false,
                Replayed,
                JournalReplayed,
            ),
            (
                "replay bits",
                sentinel(3),
                RunError::None,
                false,
                Replayed,
                JournalReplayed,
            ),
            ("failed 4", sentinel(4), RunError::None, false, Failed, E2fsck4),
            ("failed 5", sentinel(5), RunError::None, false, Failed, E2fsckOther),
            ("failed 6", sentinel(6), RunError::None, false, Failed, E2fsckOther),
            ("failed 8", sentinel(8), RunError::None, false, Failed, E2fsck8),
            ("failed 12", sentinel(12), RunError::None, false, Failed, E2fsckOther),
            ("failed 125", sentinel(125), RunError::None, false, Failed, E2fsckOther),
            ("killed 137", sentinel(137), RunError::None, false, Failed, E2fsckOther),
            (
                "not executable",
                sentinel(126),
                RunError::None,
                false,
                FailedOpen,
                ExecFailure,
            ),
            (
                "missing e2fsck",
                sentinel(127),
                RunError::None,
                false,
                FailedOpen,
                ExecFailure,
            ),
            (
                "launcher exit 1",
                Vec::new(),
                exit(1),
                false,
                FailedOpen,
                LauncherFailure,
            ),
            ("signalled -1", Vec::new(), exit(-1), false, Failed, Killed),
            ("signalled 137", Vec::new(), exit(137), false, Failed, Killed),
            (
                "timeout killed",
                Vec::new(),
                RunError::Other("signal: killed".to_owned()),
                true,
                Failed,
                Timeout,
            ),
            ("timeout nothing", Vec::new(), RunError::None, true, Failed, Timeout),
            ("sentinel lost", Vec::new(), RunError::None, false, Failed, NoSentinel),
            (
                "device refused",
                Vec::new(),
                RunError::Refused(format!("{DEVICE_REFUSED} \"/dev/sda\"")),
                false,
                Failed,
                NoSentinel,
            ),
            (
                "unrecognized",
                Vec::new(),
                RunError::Other("dbus boom".to_owned()),
                false,
                Failed,
                NoSentinel,
            ),
        ];
        for (name, sentinel, error, timed_out, outcome, reason) in cases {
            let recovery = classify(&error, &sentinel, timed_out, JournalState::Clean);

            assert_eq!(outcome_and_reason(&recovery), (outcome, reason), "{name}");
            assert_eq!(recovery.error.is_some(), outcome == Failed, "{name}");
            assert!(
                recovery
                    .error
                    .as_deref()
                    .is_none_or(|error| error.starts_with(RECOVERY_FAILED)),
                "{name}"
            );
        }
    }

    #[test]
    fn the_error_texts_match_go() {
        let text = |error: RunError, sentinel: Vec<u8>, timed_out: bool| {
            classify(&error, &sentinel, timed_out, JournalState::Clean)
                .error
                .unwrap()
        };

        assert_eq!(
            text(RunError::None, sentinel(8), false),
            "rootfs filesystem recovery did not complete: e2fsck exit 8"
        );
        assert_eq!(
            text(RunError::None, Vec::new(), true),
            "rootfs filesystem recovery did not complete: e2fsck did not report a result: recovery timed out with no result"
        );
        assert_eq!(
            text(exit(137), Vec::new(), false),
            "rootfs filesystem recovery did not complete: recovery unit signalled: exit status 137"
        );
        assert_eq!(
            text(RunError::None, Vec::new(), false),
            "rootfs filesystem recovery did not complete: e2fsck did not report a result"
        );
    }

    #[test]
    fn journal_replay_never_condemns_a_snapshot() {
        let journals = [JournalState::Unknown, JournalState::Clean, JournalState::Dirty];
        for timed_out in [false, true] {
            for rc in 0..256 {
                for journal in journals {
                    let recovery = classify(&RunError::None, &sentinel(rc), timed_out, journal);
                    let booting = matches!(recovery.outcome, RecoverOutcome::Replayed | RecoverOutcome::FailedOpen);
                    assert!(booting || recovery.outcome == RecoverOutcome::Failed, "rc {rc}");
                    assert_eq!(recovery.error.is_none(), booting, "rc {rc}");
                }
            }
            for error in [RunError::None, RunError::Other("boom".to_owned())] {
                let recovery = classify(&error, &[], timed_out, JournalState::Clean);
                assert_eq!(recovery.error.is_none(), recovery.outcome != RecoverOutcome::Failed);
            }
        }
    }

    #[test]
    fn the_last_sentinel_wins_and_absence_is_reported() {
        assert_eq!(e2fsck_rc(&sentinel(4)), Some(4));
        assert_eq!(e2fsck_rc(b"no sentinel here"), None);
        assert_eq!(e2fsck_rc(b"__E2FSCK_RC__0__ noise __E2FSCK_RC__4__"), Some(4));
    }

    #[test]
    fn a_journal_only_rejection_is_recognised_in_every_wording() {
        assert!(journal_only_rejected(b"Unknown extended option: journal_only\n"));
        assert!(journal_only_rejected(br#"Unrecognized extended option "journal_only""#));
        assert!(!journal_only_rejected(b"e2fsck 1.47.4 (6-Mar-2025)\n"));
        assert!(!journal_only_rejected(b""));
    }

    #[tokio::test]
    async fn the_host_e2fsck_accepts_journal_only() {
        assert!(journal_only_supported().await);
    }

    #[tokio::test]
    async fn a_non_nbd_device_is_refused_before_anything_runs() {
        let run = run_e2fsck("/dev/sda").await;
        assert_eq!(
            run.error,
            RunError::Refused("refusing to run e2fsck on unexpected device path \"/dev/sda\"".to_owned())
        );
        assert!(run.sentinel.is_empty());

        let recovery = recover_filesystem("/dev/sda").await;
        assert_eq!(recovery.outcome, RecoverOutcome::Failed);
        assert_eq!(
            recovery.error.as_deref(),
            Some(
                "rootfs filesystem recovery did not complete: refusing to run e2fsck on unexpected device path \"/dev/sda\""
            )
        );
    }

    #[test]
    fn rc_zero_is_labelled_from_the_superblock() {
        let cases = [
            (0, JournalState::Dirty, RecoverReason::JournalReplayed),
            (0, JournalState::Clean, RecoverReason::NothingToDo),
            (0, JournalState::Unknown, RecoverReason::ReplayUnknown),
            (1, JournalState::Clean, RecoverReason::JournalReplayed),
            (2, JournalState::Unknown, RecoverReason::JournalReplayed),
        ];
        for (rc, journal, reason) in cases {
            let recovery = classify(&RunError::None, &sentinel(rc), false, journal);

            assert_eq!(
                outcome_and_reason(&recovery),
                (RecoverOutcome::Replayed, reason),
                "rc {rc}"
            );
        }
    }

    fn superblock_image(magic: u16, incompat: u32) -> Vec<u8> {
        let mut image = vec![0u8; SUPERBLOCK_OFFSET as usize + SUPERBLOCK_SIZE];
        let base = SUPERBLOCK_OFFSET as usize;
        image[base + MAGIC_OFFSET..base + MAGIC_OFFSET + 2].copy_from_slice(&magic.to_le_bytes());
        image[base + INCOMPAT_OFFSET..base + INCOMPAT_OFFSET + 4].copy_from_slice(&incompat.to_le_bytes());
        image
    }

    #[tokio::test]
    async fn the_journal_state_comes_from_the_primary_superblock() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "dirty",
                Some(superblock_image(EXT4_MAGIC, 0x2C2 | INCOMPAT_RECOVER)),
                JournalState::Dirty,
            ),
            ("clean", Some(superblock_image(EXT4_MAGIC, 0x2C2)), JournalState::Clean),
            (
                "not ext4",
                Some(superblock_image(0x1234, INCOMPAT_RECOVER)),
                JournalState::Unknown,
            ),
            ("short", Some(vec![0u8; 512]), JournalState::Unknown),
            ("missing", None, JournalState::Unknown),
        ];
        for (name, image, want) in cases {
            let path = dir.path().join(name);
            if let Some(image) = image {
                std::fs::write(&path, image).unwrap();
            }

            assert_eq!(journal_state(path.to_str().unwrap()).await, want, "{name}");
        }
    }

    #[tokio::test]
    async fn a_stalled_device_gives_up_at_the_probe_bound() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("stalled");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

        let started = Instant::now();
        let state = journal_state(fifo.to_str().unwrap()).await;
        let took = started.elapsed();
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);

        assert_eq!(state, JournalState::Unknown);
        assert!(took >= PROBE_TIMEOUT - Duration::from_millis(100), "{took:?}");
        assert!(took < PROBE_TIMEOUT + Duration::from_secs(2), "{took:?}");
    }
}
