mod guest;

use std::time::Duration;

use self::guest::GuestEnvd;
use super::version::is_at_least;

const MIN_ENVD_FOR_FS_FREEZE: &str = "0.6.6";
const SYNC_MIN_TIMEOUT: Duration = Duration::from_secs(5);
const SYNC_MAX_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const SYNC_FLUSH_FLOOR_BYTES_PER_SEC: i64 = 50 * 1024 * 1024;
const FSTHAW_VIA_EXEC_TIMEOUT: Duration = Duration::from_secs(10);
const FSTHAW_ATTEMPTS: u32 = 3;
const FSTHAW_RETRY_BACKOFF: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuiesceMethod {
    Fsfreeze,
    FsfreezeExec,
    Sync,
}

impl QuiesceMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fsfreeze => "fsfreeze",
            Self::FsfreezeExec => "fsfreeze-exec",
            Self::Sync => "sync",
        }
    }

    pub fn froze(self) -> bool {
        self != Self::Sync
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preparation {
    pub frozen: bool,
    pub method: QuiesceMethod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparationFailure {
    pub error: String,
    pub method: QuiesceMethod,
}

#[derive(Debug, Clone, bon::Builder)]
pub struct GuestQuiesce {
    #[builder(into)]
    base_url: String,
    access_token: Option<String>,
    #[builder(into)]
    envd_version: String,
    ram_mb: i64,
    #[builder(default)]
    sync_timeout_override_ms: i64,
    #[builder(default)]
    freeze_via_exec: bool,
}

pub fn ram_scaled_sync_timeout(ram_mb: i64) -> Duration {
    let seconds = (ram_mb * 1024 * 1024 / SYNC_FLUSH_FLOOR_BYTES_PER_SEC).max(0) as u64;
    Duration::from_secs(seconds).clamp(SYNC_MIN_TIMEOUT, SYNC_MAX_TIMEOUT)
}

impl GuestQuiesce {
    pub fn sync_timeout(&self) -> Duration {
        match self.sync_timeout_override_ms {
            ms if ms > 0 => Duration::from_millis(ms as u64),
            _ => ram_scaled_sync_timeout(self.ram_mb),
        }
    }

    pub async fn prepare(&self) -> Result<Preparation, PreparationFailure> {
        let timeout = self.sync_timeout();
        if self.supports_fsfreeze() {
            return self
                .guest()
                .post("fsfreeze", timeout)
                .await
                .map(|()| Preparation {
                    frozen: true,
                    method: QuiesceMethod::Fsfreeze,
                })
                .map_err(|e| PreparationFailure {
                    error: format!("fsfreeze before filesystem-only pause: {e}"),
                    method: QuiesceMethod::Fsfreeze,
                });
        }
        if self.freeze_via_exec && self.has_fsfreeze_binary(timeout).await {
            return self.freeze_via_exec(timeout).await;
        }
        self.guest_sync(timeout).await
    }

    pub async fn rollback(&self, method: QuiesceMethod) {
        let thawed = match method {
            QuiesceMethod::Fsfreeze if self.supports_fsfreeze() => {
                self.guest().post("fsthaw", self.sync_timeout()).await
            }
            QuiesceMethod::Fsfreeze | QuiesceMethod::Sync => return,
            QuiesceMethod::FsfreezeExec => self.thaw_via_exec().await,
        };
        if let Err(e) = thawed {
            tracing::warn!(error = %e, "fsthaw failed");
        }
    }

    pub async fn thaw(&self, mut on_attempt: impl FnMut(u32, bool, Duration)) -> Result<(), String> {
        let native = self.supports_fsfreeze();
        let mut delay = FSTHAW_RETRY_BACKOFF;
        let mut result = Ok(());
        for attempt in 1..=FSTHAW_ATTEMPTS {
            let started = std::time::Instant::now();
            result = match native {
                true => self.guest().post("fsthaw", self.sync_timeout()).await,
                false => self.thaw_via_exec().await,
            };
            on_attempt(attempt, result.is_ok(), started.elapsed());
            let Err(e) = &result else { return Ok(()) };
            tracing::warn!(attempt, error = %e, "rootfs thaw failed");
            if attempt < FSTHAW_ATTEMPTS {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
        result
    }

    fn guest(&self) -> GuestEnvd {
        GuestEnvd {
            base_url: self.base_url.clone(),
            access_token: self.access_token.clone(),
        }
    }

    fn supports_fsfreeze(&self) -> bool {
        is_at_least(&self.envd_version, MIN_ENVD_FOR_FS_FREEZE).unwrap_or_else(|e| {
            tracing::warn!(envd_version = %self.envd_version, error = %e, "fsfreeze version gate: bad envd version");
            false
        })
    }

    async fn has_fsfreeze_binary(&self, timeout: Duration) -> bool {
        match self.guest().shell("command -v fsfreeze >/dev/null 2>&1", timeout).await {
            Ok(exit_code) => exit_code == 0,
            Err(e) => {
                tracing::warn!(error = %e, "probing guest for fsfreeze failed; falling back to guest sync");
                false
            }
        }
    }

    async fn freeze_via_exec(&self, timeout: Duration) -> Result<Preparation, PreparationFailure> {
        let failure = |error: String| PreparationFailure {
            error: format!("fsfreeze via exec before filesystem-only pause: {error}"),
            method: QuiesceMethod::FsfreezeExec,
        };
        match self.guest().shell("fsfreeze -f /", timeout).await.map_err(failure)? {
            0 => {
                tracing::info!("froze guest rootfs via envd exec API before filesystem-only pause");
                Ok(Preparation {
                    frozen: true,
                    method: QuiesceMethod::FsfreezeExec,
                })
            }
            code => Err(failure(format!("guest fsfreeze -f / exited with code {code}"))),
        }
    }

    async fn thaw_via_exec(&self) -> Result<(), String> {
        let script = "command -v fsfreeze >/dev/null 2>&1 && fsfreeze -u /";
        match self.guest().shell(script, FSTHAW_VIA_EXEC_TIMEOUT).await? {
            0 => Ok(()),
            code => Err(format!("fsfreeze -u exited {code}")),
        }
    }

    async fn guest_sync(&self, timeout: Duration) -> Result<Preparation, PreparationFailure> {
        let failure = |error: String| PreparationFailure {
            error: format!("guest sync before filesystem-only pause: {error}"),
            method: QuiesceMethod::Sync,
        };
        match self.guest().shell("sync", timeout).await.map_err(failure)? {
            0 => Ok(Preparation {
                frozen: false,
                method: QuiesceMethod::Sync,
            }),
            code => Err(failure(format!("guest sync exited with code {code}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;

    use super::*;

    const FS_FREEZE_ENVD: &str = MIN_ENVD_FOR_FS_FREEZE;

    #[derive(Default)]
    struct Calls {
        freeze: AtomicU32,
        thaw: AtomicU32,
    }

    async fn fake_envd(freeze: StatusCode, thaw_failures: u32) -> (String, Arc<Calls>) {
        let calls = Arc::new(Calls::default());
        let app = Router::new()
            .route(
                "/fsfreeze",
                post(move |State(calls): State<Arc<Calls>>| async move {
                    calls.freeze.fetch_add(1, Ordering::SeqCst);
                    (freeze, "FIFREEZE /: simulated failure")
                }),
            )
            .route(
                "/fsthaw",
                post(move |State(calls): State<Arc<Calls>>| async move {
                    match calls.thaw.fetch_add(1, Ordering::SeqCst) < thaw_failures {
                        true => (StatusCode::INTERNAL_SERVER_ERROR, "FITHAW /: simulated failure"),
                        false => (StatusCode::NO_CONTENT, ""),
                    }
                }),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        (url, calls)
    }

    fn quiesce(url: &str) -> GuestQuiesce {
        GuestQuiesce::builder()
            .base_url(url)
            .access_token("test-token".to_owned())
            .envd_version(FS_FREEZE_ENVD)
            .ram_mb(1024)
            .build()
    }

    #[test]
    fn the_sync_timeout_scales_with_ram_between_the_floor_and_the_cap() {
        assert_eq!(ram_scaled_sync_timeout(128), SYNC_MIN_TIMEOUT);
        assert_eq!(ram_scaled_sync_timeout(1024), Duration::from_secs(20));
        assert_eq!(ram_scaled_sync_timeout(128 * 1024), SYNC_MAX_TIMEOUT);
    }

    #[test]
    fn a_positive_flag_pins_the_sync_timeout_and_an_unset_one_falls_back_to_ram() {
        let pinned = GuestQuiesce::builder()
            .base_url("http://unused")
            .envd_version(FS_FREEZE_ENVD)
            .ram_mb(1024)
            .sync_timeout_override_ms(30_000)
            .build();
        let unset = GuestQuiesce::builder()
            .base_url("http://unused")
            .envd_version(FS_FREEZE_ENVD)
            .ram_mb(1024)
            .build();

        assert_eq!(pinned.sync_timeout(), Duration::from_secs(30));
        assert_eq!(unset.sync_timeout(), ram_scaled_sync_timeout(1024));
    }

    #[tokio::test]
    async fn a_failed_freeze_aborts_and_the_rollback_thaws_exactly_once() {
        let (url, calls) = fake_envd(StatusCode::INTERNAL_SERVER_ERROR, 0).await;
        let quiesce = quiesce(&url);

        let failure = quiesce.prepare().await.unwrap_err();

        assert_eq!(failure.method, QuiesceMethod::Fsfreeze);
        assert!(
            failure
                .error
                .starts_with("fsfreeze before filesystem-only pause: fsfreeze returned 500")
        );
        assert_eq!(calls.freeze.load(Ordering::SeqCst), 1);
        assert_eq!(
            calls.thaw.load(Ordering::SeqCst),
            0,
            "thaw must not run before the pause aborts"
        );
        quiesce.rollback(failure.method).await;
        assert_eq!(calls.thaw.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_successful_freeze_reports_frozen_and_does_not_thaw() {
        let (url, calls) = fake_envd(StatusCode::NO_CONTENT, 0).await;

        let prepared = quiesce(&url).prepare().await.unwrap();

        assert_eq!(
            prepared,
            Preparation {
                frozen: true,
                method: QuiesceMethod::Fsfreeze
            }
        );
        assert_eq!(calls.freeze.load(Ordering::SeqCst), 1);
        assert_eq!(calls.thaw.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_thaw_retries_a_failed_call_twice_and_succeeds_as_soon_as_one_does() {
        let (url, calls) = fake_envd(StatusCode::NO_CONTENT, 2).await;

        quiesce(&url).thaw(|_, _, _| {}).await.unwrap();

        assert_eq!(calls.thaw.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_thaw_gives_up_after_one_call_and_two_retries() {
        let (url, calls) = fake_envd(StatusCode::NO_CONTENT, u32::MAX).await;

        let thawed = quiesce(&url).thaw(|_, _, _| {}).await;

        assert!(thawed.unwrap_err().starts_with("fsthaw returned 500"));
        assert_eq!(calls.thaw.load(Ordering::SeqCst), FSTHAW_ATTEMPTS);
    }

    #[tokio::test]
    async fn an_envd_without_fsfreeze_syncs_through_the_exec_api() {
        let (url, calls) = shell_envd(0).await;
        let old_envd = GuestQuiesce::builder()
            .base_url(url)
            .envd_version("0.6.5")
            .ram_mb(1024)
            .build();

        let prepared = old_envd.prepare().await.unwrap();

        assert_eq!(
            prepared,
            Preparation {
                frozen: false,
                method: QuiesceMethod::Sync
            }
        );
        assert_eq!(calls.scripts(), ["sync"]);
    }

    #[tokio::test]
    async fn a_failing_guest_sync_aborts_the_pause() {
        let (url, _) = shell_envd(1).await;
        let old_envd = GuestQuiesce::builder()
            .base_url(url)
            .envd_version("0.6.5")
            .ram_mb(1024)
            .build();

        let failure = old_envd.prepare().await.unwrap_err();

        assert_eq!(
            failure.error,
            "guest sync before filesystem-only pause: guest sync exited with code 1"
        );
        assert_eq!(failure.method, QuiesceMethod::Sync);
    }

    #[tokio::test]
    async fn an_old_envd_freezes_through_exec_only_when_the_flag_is_on_and_the_binary_exists() {
        let (url, calls) = shell_envd(0).await;
        let old_envd = GuestQuiesce::builder()
            .base_url(url)
            .envd_version("0.6.5")
            .ram_mb(1024)
            .freeze_via_exec(true)
            .build();

        let prepared = old_envd.prepare().await.unwrap();

        assert_eq!(
            prepared,
            Preparation {
                frozen: true,
                method: QuiesceMethod::FsfreezeExec
            }
        );
        assert_eq!(
            calls.scripts(),
            ["command -v fsfreeze >/dev/null 2>&1", "fsfreeze -f /"]
        );
        old_envd.rollback(QuiesceMethod::FsfreezeExec).await;
        assert_eq!(
            calls.scripts().last().map(String::as_str),
            Some("command -v fsfreeze >/dev/null 2>&1 && fsfreeze -u /")
        );
    }

    #[tokio::test]
    async fn a_missing_fsfreeze_binary_falls_back_to_the_guest_sync() {
        let (url, calls) = shell_envd(1).await;
        let old_envd = GuestQuiesce::builder()
            .base_url(url)
            .envd_version("0.6.5")
            .ram_mb(1024)
            .freeze_via_exec(true)
            .build();

        let failure = old_envd.prepare().await.unwrap_err();

        assert_eq!(failure.method, QuiesceMethod::Sync);
        assert_eq!(calls.scripts(), ["command -v fsfreeze >/dev/null 2>&1", "sync"]);
    }

    struct Shell {
        scripts: parking_lot::Mutex<Vec<String>>,
        exit_code: i32,
    }

    impl Shell {
        fn scripts(&self) -> Vec<String> {
            self.scripts.lock().clone()
        }
    }

    async fn shell_envd(exit_code: i32) -> (String, Arc<Shell>) {
        let shell = Arc::new(Shell {
            scripts: parking_lot::Mutex::default(),
            exit_code,
        });
        let app = Router::new()
            .route(
                "/process.Process/Start",
                post(|State(shell): State<Arc<Shell>>, body: axum::body::Bytes| async move {
                    let request: serde_json::Value = serde_json::from_slice(&body[5..]).unwrap();
                    let script = request["process"]["args"][1].as_str().unwrap().to_owned();
                    shell.scripts.lock().push(script);
                    let end = serde_json::json!({"event": {"end": {"exitCode": shell.exit_code}}});
                    let mut framed = super::super::process::envelope(&end);
                    framed.extend(
                        super::super::process::envelope(&serde_json::json!({}))
                            .into_iter()
                            .enumerate()
                            .map(|(index, byte)| if index == 0 { 2 } else { byte }),
                    );
                    framed
                }),
            )
            .with_state(shell.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        (url, shell)
    }
}
