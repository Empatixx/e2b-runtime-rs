mod debugfs;
mod state;

use std::fs::{OpenOptions, Permissions};
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::time::Instant;

pub use debugfs::JailedDebugfs;
use state::{Content, EnvdState, Presence, STAT_UNPARSEABLE, Verdict, envd_absent, envd_executable, parse_envd_size};

use crate::go_os::path_error;
use crate::go_strconv::quote;

pub const ENVD_SWAP_TIMEOUT: Duration = Duration::from_secs(120);
pub const ENVD_SWAP_BUDGET: Duration = Duration::from_secs(240);
const MAX_ENVD_SIZE: i64 = 256 << 20;
const GUEST_ENVD: &str = "/usr/bin/envd";
const EXECUTABLE: u32 = 0o755;
const JAIL_WRITABLE: u32 = 0o666;
const STAGE_PREFIX: &str = ".envd-swap-";
const UNRECOVERABLE: &str = "offline envd swap failed and the original was not restored";
const TOO_LARGE: &str = "rootfs envd is too large to swap offline";
const MISSING: &str = "rootfs has no /usr/bin/envd to swap";
const SOURCE_MISSING: &str = "envd binary to install is no longer on disk";

pub trait Debugfs: Sync {
    fn stage_dir(&self) -> &Path;

    fn run(
        &self,
        phase: &str,
        script: &str,
        writable: bool,
        deadline: Instant,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapError {
    Unrecoverable(String),
    TooLarge(String),
    Missing(String),
    StatUnparseable(String),
    SourceMissing(String),
    Failed(String),
}

impl SwapError {
    pub fn message(&self) -> &str {
        match self {
            SwapError::Unrecoverable(message)
            | SwapError::TooLarge(message)
            | SwapError::Missing(message)
            | SwapError::StatUnparseable(message)
            | SwapError::SourceMissing(message)
            | SwapError::Failed(message) => message,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swapped {
    pub refire: bool,
    pub error: Option<SwapError>,
}

pub async fn swap_envd_binary(device: &str, source: &Path, stage_root: &Path) -> Swapped {
    let deadline = Instant::now() + ENVD_SWAP_BUDGET;
    match stage_dir(stage_root) {
        Ok(stage) => {
            let debugfs = JailedDebugfs {
                device: device.to_owned(),
                stage: stage.path().to_path_buf(),
            };
            swap_envd(&debugfs, source, deadline).await
        }
        Err(error) => Swapped {
            refire: false,
            error: Some(SwapError::Failed(error)),
        },
    }
}

fn stage_dir(stage_root: &Path) -> Result<tempfile::TempDir, String> {
    if !stage_root.is_absolute() {
        return Err(format!(
            "envd swap stage root must be absolute: {}",
            quote(&stage_root.to_string_lossy())
        ));
    }
    let stage = tempfile::Builder::new()
        .prefix(STAGE_PREFIX)
        .tempdir_in(stage_root)
        .map_err(|e| {
            let pattern = stage_root.join(format!("{STAGE_PREFIX}*"));
            format!("create swap stage dir: {}", path_error("mkdirtemp", &pattern, &e))
        })?;
    std::fs::set_permissions(stage.path(), Permissions::from_mode(EXECUTABLE))
        .map_err(|e| format!("chmod swap stage dir: {}", path_error("chmod", stage.path(), &e)))?;
    Ok(stage)
}

pub async fn swap_envd(debugfs: &impl Debugfs, source: &Path, deadline: Instant) -> Swapped {
    let mut refire = false;
    let error = swap_steps(debugfs, source, deadline, &mut refire).await.err();
    Swapped { refire, error }
}

async fn swap_steps(
    debugfs: &impl Debugfs,
    source: &Path,
    deadline: Instant,
    refire: &mut bool,
) -> Result<(), SwapError> {
    let staged = stage_target(debugfs.stage_dir(), source)?;
    let target_sha = sha256_of(&staged)
        .await
        .map_err(|e| SwapError::Failed(format!("hash target envd: {e}")))?;
    refuse_oversized(debugfs, deadline).await?;
    let original = back_up_original(debugfs, deadline).await?;
    let original_sha = sha256_of(&original)
        .await
        .map_err(|e| SwapError::Failed(format!("hash original envd: {e}")))?;
    *refire = original_sha == target_sha;
    let swap_context = install(debugfs, &staged, deadline).await;
    let state = read_state(debugfs, "state", &target_sha, &original_sha, decision_deadline()).await;
    decide(debugfs, &state, &original, &swap_context).await
}

fn stage_target(stage: &Path, source: &Path) -> Result<PathBuf, SwapError> {
    let staged = stage.join("envd.new");
    copy_file(source, &staged).map_err(|e| match std::fs::metadata(source) {
        Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {
            SwapError::SourceMissing(format!("{SOURCE_MISSING}: {}: {e}", quote(&source.to_string_lossy())))
        }
        _ => SwapError::Failed(format!("stage target envd {}: {e}", quote(&source.to_string_lossy()))),
    })?;
    std::fs::set_permissions(&staged, Permissions::from_mode(EXECUTABLE))
        .map_err(|e| SwapError::Failed(format!("chmod staged envd: {}", path_error("chmod", &staged, &e))))?;
    match std::fs::metadata(&staged) {
        Ok(metadata) if metadata.len() > 0 => Ok(staged),
        _ => Err(SwapError::Failed(format!(
            "refusing offline swap: staged target envd {} is empty or unreadable",
            quote(&source.to_string_lossy())
        ))),
    }
}

async fn refuse_oversized(debugfs: &impl Debugfs, deadline: Instant) -> Result<(), SwapError> {
    let size_check = |detail: &str| format!("size-check original envd: {detail}");
    let stat = debugfs
        .run("size-stat", &stat_script(), false, deadline)
        .await
        .map_err(|e| SwapError::Failed(size_check(&format!("stat for size check: {e}"))))?;
    let stat = String::from_utf8_lossy(&stat);
    if envd_absent(&stat) {
        return Err(SwapError::Missing(size_check(MISSING)));
    }
    let size = parse_envd_size(&stat).map_err(|_| SwapError::StatUnparseable(size_check(STAT_UNPARSEABLE)))?;
    match size > MAX_ENVD_SIZE {
        true => Err(SwapError::TooLarge(format!(
            "{TOO_LARGE}: {GUEST_ENVD} is {size} bytes, over the {MAX_ENVD_SIZE}-byte limit"
        ))),
        false => Ok(()),
    }
}

async fn back_up_original(debugfs: &impl Debugfs, deadline: Instant) -> Result<PathBuf, SwapError> {
    let original = debugfs.stage_dir().join("envd.orig");
    create_jail_writable(&original).map_err(|e| SwapError::Failed(format!("pre-create backup target: {e}")))?;
    debugfs
        .run("backup", &dump_script(&original), false, deadline)
        .await
        .map_err(|e| SwapError::Failed(format!("back up original envd: {e}")))?;
    if std::fs::metadata(&original).map_or(true, |metadata| metadata.len() == 0) {
        return Err(SwapError::Failed(format!(
            "refusing offline swap: could not read original {GUEST_ENVD} to back up (dump produced nothing)"
        )));
    }
    std::fs::set_permissions(&original, Permissions::from_mode(EXECUTABLE))
        .map_err(|e| SwapError::Failed(format!("chmod original backup: {}", path_error("chmod", &original, &e))))?;
    Ok(original)
}

async fn install(debugfs: &impl Debugfs, binary: &Path, deadline: Instant) -> String {
    match debugfs.run("swap", &write_script(binary), true, deadline).await {
        Ok(_) => "debugfs exited cleanly".to_owned(),
        Err(e) => format!("debugfs errored: {e}"),
    }
}

async fn decide(
    debugfs: &impl Debugfs,
    state: &EnvdState,
    original: &Path,
    swap_context: &str,
) -> Result<(), SwapError> {
    match state.verdict() {
        Verdict::Applied => Ok(()),
        Verdict::OriginalIntact => Err(SwapError::Failed(format!(
            "offline envd swap did not apply; original left in place ({swap_context})"
        ))),
        Verdict::Damaged => match roll_back(debugfs, original).await {
            Ok(()) => Err(SwapError::Failed(format!(
                "offline envd swap did not land (envd was {}, {swap_context}); original envd restored",
                state.describe()
            ))),
            Err(rollback) => Err(SwapError::Unrecoverable(format!(
                "{UNRECOVERABLE}: swap left envd damaged and rollback failed ({swap_context}): {rollback}"
            ))),
        },
        Verdict::Unknown => Err(SwapError::Unrecoverable(format!(
            "{UNRECOVERABLE}: swap did not land and rootfs state is indeterminate ({swap_context}): {}",
            state.describe()
        ))),
    }
}

async fn roll_back(debugfs: &impl Debugfs, original: &Path) -> Result<(), String> {
    let rewritten = debugfs
        .run("rollback", &write_script(original), true, decision_deadline())
        .await;
    let original_sha = sha256_of(original)
        .await
        .map_err(|e| format!("hash original envd for rollback check: {e}"))?;
    let restored = read_state(
        debugfs,
        "rollback-verify",
        &original_sha,
        &original_sha,
        decision_deadline(),
    )
    .await;
    match (restored.verdict(), rewritten) {
        (Verdict::Applied, _) => Ok(()),
        (_, Err(e)) => Err(format!(
            "restored envd is {} (restore debugfs errored: {e})",
            restored.describe()
        )),
        (_, Ok(_)) => Err(format!("restored envd is {}", restored.describe())),
    }
}

async fn read_state(
    debugfs: &impl Debugfs,
    phase: &str,
    target_sha: &str,
    original_sha: &str,
    deadline: Instant,
) -> EnvdState {
    let stat = match debugfs
        .run(&format!("{phase}-stat"), &stat_script(), false, deadline)
        .await
    {
        Ok(stat) => String::from_utf8_lossy(&stat).into_owned(),
        Err(e) => {
            return EnvdState {
                stat_error: Some(e),
                ..EnvdState::unread(Presence::Unknown)
            };
        }
    };
    let presence = match (envd_absent(&stat), envd_executable(&stat)) {
        (true, _) => return EnvdState::unread(Presence::Absent),
        (false, true) => Presence::Executable,
        (false, false) => Presence::NotExecutable,
    };
    match dump_sha(debugfs, phase, deadline).await {
        Err(e) => EnvdState {
            dump_error: Some(e),
            ..EnvdState::unread(presence)
        },
        Ok(sha) => EnvdState {
            content: content_of(&sha, target_sha, original_sha),
            sha,
            ..EnvdState::unread(presence)
        },
    }
}

fn content_of(sha: &str, target_sha: &str, original_sha: &str) -> Content {
    match sha {
        sha if sha == target_sha => Content::Target,
        sha if sha == original_sha => Content::Original,
        _ => Content::Other,
    }
}

async fn dump_sha(debugfs: &impl Debugfs, phase: &str, deadline: Instant) -> Result<String, String> {
    let dumped = debugfs.stage_dir().join(format!("envd.{phase}"));
    create_jail_writable(&dumped).map_err(|e| format!("pre-create dump target: {e}"))?;
    debugfs
        .run(phase, &dump_script(&dumped), false, deadline)
        .await
        .map_err(|e| format!("dump {GUEST_ENVD}: {e}"))?;
    let metadata = std::fs::metadata(&dumped)
        .map_err(|e| format!("stat dump of {GUEST_ENVD}: {}", path_error("stat", &dumped, &e)))?;
    if metadata.len() == 0 {
        return Err(format!(
            "dump of {GUEST_ENVD} produced no bytes (silent debugfs failure)"
        ));
    }
    sha256_of(&dumped).await
}

fn decision_deadline() -> Instant {
    Instant::now() + ENVD_SWAP_TIMEOUT
}

fn stat_script() -> String {
    format!("stat {GUEST_ENVD}\n")
}

fn dump_script(target: &Path) -> String {
    format!("dump {GUEST_ENVD} {}\n", target.display())
}

fn write_script(binary: &Path) -> String {
    format!(
        "rm {GUEST_ENVD}\nwrite {} {GUEST_ENVD}\nsif {GUEST_ENVD} mode 0100755\n",
        binary.display()
    )
}

fn create_jail_writable(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(JAIL_WRITABLE)
        .open(path)
        .map_err(|e| path_error("open", path, &e))?;
    std::fs::set_permissions(path, Permissions::from_mode(JAIL_WRITABLE)).map_err(|e| path_error("chmod", path, &e))
}

fn copy_file(source: &Path, target: &Path) -> Result<(), String> {
    let mut input = std::fs::File::open(source).map_err(|e| path_error("open", source, &e))?;
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(EXECUTABLE)
        .open(target)
        .map_err(|e| path_error("open", target, &e))?;
    std::io::copy(&mut input, &mut output)
        .map(drop)
        .map_err(|e| path_error("read", source, &e))
}

async fn sha256_of(path: &Path) -> Result<String, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path).map_err(|e| path_error("open", &path, &e))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buffer).map_err(|e| path_error("read", &path, &e))? {
                0 => return Ok(hex::encode(hasher.finalize())),
                read => hasher.update(&buffer[..read]),
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use parking_lot::Mutex;

    use super::*;

    const STAT_NOT_FOUND: &str = "/usr/bin/envd: File not found by ext2_lookup\n";
    const TENANT_MARKER: &str = "TENANT-CRAFTED-OUTPUT-marker-2f9c";

    fn stat_found(mode: &str, size: i64) -> String {
        format!(
            "Inode: 12   Type: regular    Mode:  {mode}   Flags: 0x80000\nUser:     0   Group:     0   Project:     0   Size: {size}\n"
        )
    }

    #[derive(Default)]
    struct FakeDebugfs {
        stage: PathBuf,
        stdout: HashMap<&'static str, String>,
        dump_body: HashMap<&'static str, &'static str>,
        errors: HashMap<&'static str, &'static str>,
        phases: Mutex<Vec<String>>,
    }

    impl FakeDebugfs {
        fn ran(&self, phase: &str) -> bool {
            self.phases.lock().iter().any(|ran| ran == phase)
        }
    }

    impl Debugfs for FakeDebugfs {
        fn stage_dir(&self) -> &Path {
            &self.stage
        }

        async fn run(&self, phase: &str, script: &str, _writable: bool, _deadline: Instant) -> Result<Vec<u8>, String> {
            self.phases.lock().push(phase.to_owned());
            let dump_target = script
                .trim()
                .strip_prefix("dump ")
                .and_then(|rest| rest.split_whitespace().nth(1));
            if let (Some(target), Some(body)) = (dump_target, self.dump_body.get(phase)) {
                std::fs::write(target, body).unwrap();
            }
            match self.errors.get(phase) {
                Some(error) => Err((*error).to_owned()),
                None => Ok(self.stdout.get(phase).cloned().unwrap_or_default().into_bytes()),
            }
        }
    }

    struct Fixture {
        _dirs: (tempfile::TempDir, tempfile::TempDir),
        debugfs: FakeDebugfs,
        source: PathBuf,
    }

    fn fixture(target: &str) -> Fixture {
        let stage = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        let source = sources.path().join("envd.src");
        std::fs::write(&source, target).unwrap();
        let mut debugfs = FakeDebugfs {
            stage: stage.path().to_path_buf(),
            ..FakeDebugfs::default()
        };
        debugfs.stdout.insert("size-stat", stat_found("0755", 4096));
        debugfs.dump_body.insert("backup", "ORIGINAL-BINARY");
        Fixture {
            _dirs: (stage, sources),
            debugfs,
            source,
        }
    }

    async fn swap(fixture: &Fixture) -> Swapped {
        swap_envd(&fixture.debugfs, &fixture.source, Instant::now() + ENVD_SWAP_BUDGET).await
    }

    fn restored_original(fixture: &mut Fixture) {
        fixture
            .debugfs
            .stdout
            .insert("rollback-verify-stat", stat_found("0755", 15));
        fixture.debugfs.dump_body.insert("rollback-verify", "ORIGINAL-BINARY");
    }

    #[tokio::test]
    async fn an_absent_binary_is_rolled_back_without_dumping_it() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.stdout.insert("state-stat", STAT_NOT_FOUND.to_owned());
        restored_original(&mut fixture);

        let swapped = swap(&fixture).await;

        let error = swapped.error.unwrap();
        assert!(matches!(error, SwapError::Failed(_)), "{error:?}");
        assert!(error.message().contains("original envd restored"), "{error:?}");
        assert!(fixture.debugfs.ran("rollback"));
        assert!(!fixture.debugfs.ran("state"));
    }

    #[tokio::test]
    async fn an_unreadable_rootfs_is_never_rolled_back() {
        let mut stat_fails = fixture("NEW-BINARY");
        stat_fails.debugfs.errors.insert("state-stat", "jail failed to start");
        let mut dump_empty = fixture("NEW-BINARY");
        dump_empty.debugfs.stdout.insert("state-stat", stat_found("0755", 4096));

        for fixture in [stat_fails, dump_empty] {
            let swapped = swap(&fixture).await;

            assert!(
                matches!(swapped.error, Some(SwapError::Unrecoverable(_))),
                "{:?}",
                swapped.error
            );
            assert!(!fixture.debugfs.ran("rollback"));
        }
    }

    #[tokio::test]
    async fn a_landed_swap_succeeds_and_moved_the_sandbox() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.stdout.insert("state-stat", stat_found("0755", 10));
        fixture.debugfs.dump_body.insert("state", "NEW-BINARY");

        let swapped = swap(&fixture).await;

        assert_eq!(swapped, Swapped::default());
        assert!(!fixture.debugfs.ran("rollback"));
    }

    #[tokio::test]
    async fn an_untouched_original_boots_without_a_rollback() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.errors.insert("swap", "context deadline exceeded");
        fixture.debugfs.stdout.insert("state-stat", stat_found("0755", 15));
        fixture.debugfs.dump_body.insert("state", "ORIGINAL-BINARY");

        let error = swap(&fixture).await.error.unwrap();

        assert_eq!(
            error,
            SwapError::Failed(
                "offline envd swap did not apply; original left in place (debugfs errored: context deadline exceeded)"
                    .to_owned()
            )
        );
        assert!(!fixture.debugfs.ran("rollback"));
    }

    #[tokio::test]
    async fn a_target_without_the_exec_bit_is_rolled_back() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.stdout.insert("state-stat", stat_found("0644", 10));
        fixture.debugfs.dump_body.insert("state", "NEW-BINARY");
        restored_original(&mut fixture);

        let error = swap(&fixture).await.error.unwrap();

        assert!(error.message().contains("original envd restored"), "{error:?}");
        assert!(fixture.debugfs.ran("rollback"));
    }

    #[tokio::test]
    async fn a_rootfs_that_already_held_the_target_is_a_refire() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.dump_body.insert("backup", "NEW-BINARY");
        fixture.debugfs.stdout.insert("state-stat", stat_found("0755", 10));
        fixture.debugfs.dump_body.insert("state", "NEW-BINARY");

        let swapped = swap(&fixture).await;

        assert_eq!(
            swapped,
            Swapped {
                refire: true,
                error: None
            }
        );
    }

    #[tokio::test]
    async fn an_oversized_envd_is_refused_before_anything_is_read() {
        let mut fixture = fixture("NEW-BINARY");
        fixture
            .debugfs
            .stdout
            .insert("size-stat", stat_found("0755", MAX_ENVD_SIZE + 1));

        let swapped = swap(&fixture).await;

        assert_eq!(
            swapped.error,
            Some(SwapError::TooLarge(
                "rootfs envd is too large to swap offline: /usr/bin/envd is 268435457 bytes, over the 268435456-byte limit"
                    .to_owned()
            ))
        );
        assert!(!swapped.refire);
        assert!(!fixture.debugfs.ran("backup"));
    }

    #[tokio::test]
    async fn the_rollback_trusts_the_read_back_not_the_process_exit() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.stdout.insert("state-stat", STAT_NOT_FOUND.to_owned());
        fixture.debugfs.errors.insert("rollback", "signal: killed");
        restored_original(&mut fixture);

        let error = swap(&fixture).await.error.unwrap();

        assert!(matches!(error, SwapError::Failed(_)), "{error:?}");
        assert!(error.message().contains("original envd restored"), "{error:?}");
    }

    #[tokio::test]
    async fn a_failed_rollback_is_unrecoverable() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.stdout.insert("state-stat", STAT_NOT_FOUND.to_owned());
        fixture
            .debugfs
            .stdout
            .insert("rollback-verify-stat", STAT_NOT_FOUND.to_owned());

        let error = swap(&fixture).await.error.unwrap();

        assert!(matches!(error, SwapError::Unrecoverable(_)), "{error:?}");
        assert!(fixture.debugfs.ran("rollback"));
    }

    #[tokio::test]
    async fn refusals_name_the_rootfs_property_and_touch_nothing() {
        let mut missing = fixture("NEW-BINARY");
        missing.debugfs.stdout.insert("size-stat", STAT_NOT_FOUND.to_owned());
        let mut unparseable = fixture("NEW-BINARY");
        unparseable
            .debugfs
            .stdout
            .insert("size-stat", format!("{TENANT_MARKER} Size: garbage"));

        let missing_error = swap(&missing).await.error;
        let unparseable_error = swap(&unparseable).await.error;

        assert_eq!(
            missing_error,
            Some(SwapError::Missing(
                "size-check original envd: rootfs has no /usr/bin/envd to swap".to_owned()
            ))
        );
        assert_eq!(
            unparseable_error,
            Some(SwapError::StatUnparseable(
                "size-check original envd: debugfs stat output has no parseable size".to_owned()
            ))
        );
        assert!(!missing.debugfs.ran("backup") && !unparseable.debugfs.ran("backup"));
    }

    #[tokio::test]
    async fn errors_carry_no_tenant_output() {
        let mut fixture = fixture("NEW-BINARY");
        fixture.debugfs.stdout.insert("size-stat", TENANT_MARKER.to_owned());
        fixture.debugfs.errors.insert("size-stat", "jail failed to start");

        let error = swap(&fixture).await.error.unwrap();

        assert!(!error.message().contains(TENANT_MARKER), "{error:?}");
        assert!(error.message().contains("size-check original envd"), "{error:?}");
    }

    #[tokio::test]
    async fn a_retired_source_is_named_and_an_empty_one_refused() {
        let mut gone = fixture("NEW-BINARY");
        gone.source = gone.source.with_file_name("retired");
        let empty = fixture("");

        let gone_error = swap(&gone).await.error.unwrap();
        let empty_error = swap(&empty).await.error.unwrap();

        assert!(matches!(gone_error, SwapError::SourceMissing(_)), "{gone_error:?}");
        assert!(
            gone_error
                .message()
                .starts_with("envd binary to install is no longer on disk: "),
            "{gone_error:?}"
        );
        assert!(
            empty_error
                .message()
                .starts_with("refusing offline swap: staged target envd"),
            "{empty_error:?}"
        );
    }

    #[tokio::test]
    async fn a_relative_stage_root_is_refused() {
        let swapped = swap_envd_binary("/dev/nbd0", Path::new("/fc-envd/envd"), Path::new("relative")).await;

        assert_eq!(
            swapped.error,
            Some(SwapError::Failed(
                "envd swap stage root must be absolute: \"relative\"".to_owned()
            ))
        );
    }
}
