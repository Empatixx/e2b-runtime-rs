pub mod request;
mod resume_freeze;

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Serialize;
use subtle::ConstantTimeEq;

use self::request::InitRequest;
use crate::app::AppState;
use crate::host;
use crate::host::mmds_route::pin_mmds_route;
use crate::util::rate_limit::RateLimit;

const VERSION_HEADER: &str = "X-Envd-Version";
const MEMORY_HEADER: &str = "X-Envd-Memory";
const DEFAULTS_HEADER: &str = "X-Envd-Defaults";
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

pub async fn post_init(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let mut response = respond(&state, &body).await;
    let headers = response.headers_mut();
    headers.insert(VERSION_HEADER, HeaderValue::from_static(env!("CARGO_PKG_VERSION")));
    if let Some(memory) = serde_json::to_string(&state.memory)
        .ok()
        .and_then(|memory| memory.parse().ok())
    {
        headers.insert(MEMORY_HEADER, memory);
    }
    response
}

async fn respond(state: &Arc<AppState>, body: &[u8]) -> Response {
    let request = match body.is_empty() {
        true => Ok(InitRequest::default()),
        false => request::parse(body),
    };
    let request = match request {
        Ok(request) => request,
        Err(e) => {
            tracing::error!("Failed to decode request: {e}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let _guard = state.init_lock.lock().await;
    let mmds_hash = mmds_hash(state).await;
    if let Err(e) = validate_token(
        state.access_token().as_deref(),
        request.access_token.as_deref(),
        mmds_hash.as_deref(),
    ) {
        return (StatusCode::UNAUTHORIZED, e).into_response();
    }
    let freeze_audit = resume_freeze::audit_header(state);
    let applied = match is_newest(state, request.timestamp) {
        true => apply(state, request).await,
        false => Ok(()),
    };
    resume_freeze::unfreeze_user_cgroups(state).await;
    let mut response = match applied {
        Ok(()) => initialized(state),
        Err(e) => {
            tracing::error!("Failed to set data: {e}");
            (StatusCode::BAD_REQUEST, e).into_response()
        }
    };
    if let Some(audit) = freeze_audit.and_then(|audit| audit.parse().ok()) {
        response.headers_mut().insert(resume_freeze::FREEZE_AUDIT_HEADER, audit);
    }
    response
}

fn initialized(state: &Arc<AppState>) -> Response {
    state.initialized.store(true, Ordering::SeqCst);
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
    (
        StatusCode::NO_CONTENT,
        [("cache-control", "no-store"), (DEFAULTS_HEADER, reported.as_str())],
    )
        .into_response()
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

async fn apply(state: &Arc<AppState>, request: InitRequest) -> Result<(), String> {
    if let Some(timestamp) = request.timestamp {
        host::sync_clock(timestamp);
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
        state
            .nfs
            .setup(request.lifecycle_id.as_deref(), &mounts)
            .await
            .map_err(|e| format!("failed to setup NFS volumes: {e}"))?;
    }
    Ok(())
}

fn setup_hyperloop(state: &AppState, address: &str) {
    let _locked = state.hyperloop_lock.lock();
    let rewritten = address
        .parse::<IpAddr>()
        .map_err(|_| {
            format!("failed to get ip family: failed to parse IP address: ParseAddr({address:?}): unable to parse IP")
        })
        .and_then(|_| host::hosts::rewrite_hosts_file(address, Path::new("/etc/hosts")).map_err(|e| e.to_string()));
    match rewritten {
        Ok(()) => state.env_vars.store(EVENTS_ADDRESS, &format!("http://{address}")),
        Err(e) => tracing::error!(error = %e, "failed to modify hosts file"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
