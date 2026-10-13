pub mod presence;
pub mod request;
mod resume_freeze;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Serialize;
use subtle::ConstantTimeEq;

use self::presence::{Client, Presence};
use self::request::InitRequest;
use crate::app::AppState;
use crate::host;
use crate::host::mmds_route::pin_mmds_route;
use crate::util::go_json;
use crate::util::rate_limit::RateLimit;

const VERSION_HEADER: &str = "X-Envd-Version";
const MEMORY_HEADER: &str = "X-Envd-Memory";
const DEFAULTS_HEADER: &str = "X-Envd-Defaults";
const HANDOVER_HEADER: &str = "X-Envd-Handover";
const CPUS_HEADER: &str = "X-Envd-Cpus";
const THAW_HEADER: &str = "X-Envd-Thaw";
const CLOCK_CORRECTION_HEADER: &str = "X-Envd-Init-Clock-Correction";
const UNOBSERVED_HEADER: &str = "X-Envd-Init-Unobserved";
const SKIP_THAW: &str = "skip";
const EVENTS_ADDRESS: &str = "E2B_EVENTS_ADDRESS";
const MMDS_POLL_LIMIT: Duration = Duration::from_secs(60);

static PIN_MMDS_WARN: RateLimit = RateLimit::new(Duration::from_secs(10));

pub fn validate_token(
    current: Option<&str>,
    requested: Option<&str>,
    mmds_hash: Option<&str>,
) -> Result<(), &'static str> {
    if let (Some(current), Some(requested)) = (current, requested)
        && bool::from(current.as_bytes().ct_eq(requested.as_bytes()))
    {
        return Ok(());
    }
    if mmds_hash.is_some_and(|hash| hash == host::hash_access_token(requested.unwrap_or_default())) {
        return Ok(());
    }
    if current.is_none() && mmds_hash.is_none() {
        return Ok(());
    }
    if requested.is_none() {
        return Err("access token reset not authorized");
    }
    Err("access token validation failed")
}

pub async fn post_init(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let client = Arc::new(Client::default());
    let _watch = client.watch();
    serve(&state, request.into_body(), client).await
}

pub async fn serve(state: &Arc<AppState>, body: Body, presence: Arc<dyn Presence>) -> Response {
    let mut headers = entry_headers(state);
    let entry_generation = state.freezer.freeze_generation().await;
    let mut response = respond(state, body, entry_generation, presence, &mut headers).await;
    response.headers_mut().extend(headers);
    response
}

fn entry_headers(state: &AppState) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(VERSION_HEADER, HeaderValue::from_static(env!("CARGO_PKG_VERSION")));
    if let Some(memory) = serde_json::to_string(&state.memory)
        .ok()
        .and_then(|memory| memory.parse().ok())
    {
        headers.insert(MEMORY_HEADER, memory);
    }
    let handover = state.handover.lock().clone();
    if let Some(report) = handover.and_then(|report| serde_json::to_string(&report).ok()?.parse().ok()) {
        headers.insert(HANDOVER_HEADER, report);
    }
    headers
}

async fn respond(
    state: &Arc<AppState>,
    body: Body,
    entry_generation: u64,
    presence: Arc<dyn Presence>,
    headers: &mut HeaderMap,
) -> Response {
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("Failed to read request body: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let body_read = state.clock.now();
    let mut request = match decode(&body) {
        Ok(request) => request,
        Err(e) => {
            tracing::error!("Failed to decode request: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    if presence.gone() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let _guard = state.init_lock.lock().await;
    let mmds_hash = mmds_hash(state).await;
    if let Err(e) = validate_token(
        state.access_token().as_deref(),
        request.access_token.as_deref(),
        mmds_hash.as_deref(),
    ) {
        return (StatusCode::UNAUTHORIZED, e).into_response();
    }
    if let Some(audit) = resume_freeze::audit_header(state).and_then(|audit| audit.parse().ok()) {
        headers.insert(resume_freeze::FREEZE_AUDIT_HEADER, audit);
    }
    let skip_thaw = request.thaw.as_deref() == Some(SKIP_THAW);
    if skip_thaw {
        headers.insert(THAW_HEADER, HeaderValue::from_static(SKIP_THAW));
    }
    request.timestamp = request
        .timestamp
        .map(|timestamp| correct_timestamp(state, timestamp, body_read, headers));
    let applied = match is_newest(state, request.timestamp) {
        true => apply(state, request).await,
        false => Ok(None),
    };
    if applied.is_ok() {
        state.initialized.store(true, Ordering::SeqCst);
    }
    if !skip_thaw {
        resume_freeze::thaw_workload(state, entry_generation, presence.clone()).await;
    }
    match applied {
        Ok(cpu_rejection) => initialized(state, cpu_rejection, presence.as_ref(), headers),
        Err(e) => {
            tracing::error!("Failed to set data: {e}");
            (StatusCode::BAD_REQUEST, e).into_response()
        }
    }
}

fn decode(body: &[u8]) -> Result<InitRequest, String> {
    match body.is_empty() {
        true => Ok(InitRequest::default()),
        false => request::parse(body),
    }
}

fn correct_timestamp(
    state: &AppState,
    timestamp: DateTime<Utc>,
    body_read: Instant,
    headers: &mut HeaderMap,
) -> DateTime<Utc> {
    let correction = state.clock.now().saturating_duration_since(body_read);
    headers.insert(
        CLOCK_CORRECTION_HEADER,
        HeaderValue::from(correction.as_millis() as u64),
    );
    tracing::info!(
        clock_correction = milliseconds(correction),
        "corrected the /init timestamp by the handler's delay"
    );
    chrono::Duration::from_std(correction)
        .ok()
        .and_then(|correction| timestamp.checked_add_signed(correction))
        .unwrap_or(timestamp)
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_nanos() as f64 / 1e6
}

fn initialized(
    state: &Arc<AppState>,
    cpu_rejection: Option<String>,
    presence: &dyn Presence,
    headers: &mut HeaderMap,
) -> Response {
    if !presence.gone() {
        headers.insert(
            UNOBSERVED_HEADER,
            HeaderValue::from(state.freezer.take_unobserved_inits()),
        );
    }
    let poller = state.clone();
    tokio::spawn(async move {
        let opts = host::mmds::poll_for_opts(&poller.env_vars, Some(MMDS_POLL_LIMIT)).await;
        poller.deliver_mmds(opts);
    });
    let defaults = state.defaults();
    let effective = EffectiveDefaults {
        user: defaults.user,
        workdir: defaults.workdir,
        fallback: !defaults.user_delivered,
    };
    let reported = serde_json::to_string(&effective).unwrap_or_default();
    let mut response = (
        StatusCode::NO_CONTENT,
        [("cache-control", "no-store"), (DEFAULTS_HEADER, reported.as_str())],
    )
        .into_response();
    if let Some(report) = cpu_report(state, cpu_rejection).and_then(|report| report.parse().ok()) {
        response.headers_mut().insert(CPUS_HEADER, report);
    }
    response
}

fn cpu_report(state: &AppState, rejected: Option<String>) -> Option<String> {
    let status = state.cpus.status();
    let report = CpuReport {
        online: status.online,
        possible: status.possible,
        target: status.target,
        rejected,
    };
    serde_json::to_string(&report)
        .inspect_err(|e| tracing::warn!(error = %e, "could not encode the cpu state report"))
        .ok()
        .map(|json| go_json::escape_html(&json))
}

async fn mmds_hash(state: &AppState) -> Option<String> {
    if state.config.is_not_fc {
        return None;
    }
    let opts = match host::mmds::get_opts().await {
        Ok(opts) => Ok(opts),
        Err(_) => {
            if let Err(e) = pin_mmds_route().await
                && let Some(suppressed) = PIN_MMDS_WARN.allow()
            {
                tracing::warn!(error = %e, suppressed, "failed to pin MMDS iptables route");
            }
            host::mmds::get_opts().await
        }
    };
    opts.ok()
        .map(|opts| opts.access_token_hash)
        .filter(|hash| !hash.is_empty())
}

#[derive(Serialize)]
struct CpuReport {
    online: usize,
    possible: usize,
    target: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    rejected: Option<String>,
}

#[derive(Serialize)]
struct EffectiveDefaults {
    user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workdir: Option<String>,
    fallback: bool,
}

fn is_newest(state: &AppState, timestamp: Option<DateTime<Utc>>) -> bool {
    timestamp
        .map(|timestamp| timestamp.timestamp_nanos_opt().unwrap_or(i64::MAX))
        .is_none_or(|nanos| state.last_set_time.fetch_max(nanos, Ordering::SeqCst) <= nanos)
}

async fn apply(state: &Arc<AppState>, request: InitRequest) -> Result<Option<String>, String> {
    if let Some(timestamp) = request.timestamp {
        host::sync_clock(state.clock.as_ref(), timestamp);
    }
    if let Some(vars) = &request.env_vars {
        state.env_vars.replace_user_vars(vars);
    }
    *state.access_token.write() = request.access_token;
    if let Some(address) = request.hyperloop_ip {
        let state = state.clone();
        std::thread::spawn(move || setup_hyperloop(&state, &address));
    }
    {
        let mut defaults = state.defaults.write();
        if let Some(user) = request.default_user.filter(|user| !user.is_empty()) {
            defaults.user = user;
            defaults.user_delivered = true;
        }
        if let Some(workdir) = request.default_workdir.filter(|workdir| !workdir.is_empty()) {
            defaults.workdir = Some(workdir);
        }
    }
    if let Some(bundle) = request.ca_bundle.filter(|bundle| !bundle.is_empty()) {
        state
            .ca_certs
            .install(&bundle)
            .await
            .map_err(|e| format!("failed to install CA bundle: {e}"))?;
    }
    if let Some(mounts) = request.volume_mounts {
        let mounting = state.clone();
        let lifecycle_id = request.lifecycle_id.clone();
        tokio::spawn(async move { mounting.nfs.setup(lifecycle_id.as_deref(), &mounts).await })
            .await
            .map_err(|e| e.to_string())
            .and_then(|mounted| mounted)
            .map_err(|e| format!("failed to setup NFS volumes: {e}"))?;
    }
    Ok(request.cpu_count.and_then(|count| set_cpu_target(state, count)))
}

fn set_cpu_target(state: &AppState, count: i64) -> Option<String> {
    state
        .cpus
        .set_target(count)
        .inspect_err(|e| tracing::warn!(error = %e, cpu_count = count, "ignoring cpu count"))
        .err()
}

fn setup_hyperloop(state: &AppState, address: &str) {
    let _locked = state.hyperloop_lock.lock();
    match host::hosts::rewrite_hosts_file(address, Path::new("/etc/hosts")) {
        Ok(()) => state.env_vars.store(EVENTS_ADDRESS, &format!("http://{address}")),
        Err(e) => tracing::error!(error = %e, "failed to modify hosts file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logged_correction_is_the_exact_term_in_milliseconds() {
        assert_eq!(milliseconds(Duration::from_micros(300_900)), 300.9);
        assert_eq!(milliseconds(Duration::from_secs(1)), 1000.0);
    }

    #[test]
    fn token_rules() {
        let t2_hash = host::hash_access_token("t2");
        let t2_hash = Some(t2_hash.as_str());
        let other_hash = Some("other");

        assert_eq!(validate_token(Some("t1"), Some("t1"), None), Ok(()));
        assert_eq!(validate_token(Some("t1"), Some("t2"), t2_hash), Ok(()));
        assert_eq!(validate_token(None, Some("t1"), None), Ok(()));
        assert_eq!(validate_token(None, None, None), Ok(()));
        assert_eq!(
            validate_token(Some("t1"), None, None),
            Err("access token reset not authorized")
        );
        assert_eq!(
            validate_token(Some("t1"), Some("t2"), None),
            Err("access token validation failed")
        );
        assert_eq!(
            validate_token(None, Some("t2"), other_hash),
            Err("access token validation failed")
        );
    }
}
