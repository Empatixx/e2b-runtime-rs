//! `POST /init` — the orchestrator's first call after the VM starts or resumes (upstream `api/init.go`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::host;
use crate::state::AppState;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitRequest {
    pub env_vars: Option<BTreeMap<String, String>>,
    pub access_token: Option<String>,
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
    pub default_user: Option<String>,
    pub default_workdir: Option<String>,
    #[serde(rename = "hyperloopIP")]
    pub hyperloop_ip: Option<String>,
    pub ca_bundle: Option<String>,
    pub volume_mounts: Option<serde_json::Value>,
    #[serde(rename = "lifecycleID")]
    pub lifecycle_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitError {
    /// "access token validation failed"
    Mismatch,
    /// "access token reset not authorized"
    ResetNotAuthorized,
}

impl InitError {
    pub fn message(&self) -> &'static str {
        match self {
            InitError::Mismatch => "access token validation failed",
            InitError::ResetNotAuthorized => "access token reset not authorized",
        }
    }
}

/// Result of comparing the request token with the hash the orchestrator put into MMDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmdsCheck {
    /// MMDS hash equals hash(request token).
    pub matches: bool,
    /// MMDS answered with a non-empty hash at all.
    pub exists: bool,
}

/// Decides whether `/init` may set `request` as the new token (upstream `validateInitAccessToken`).
/// `existing` = token currently set; `request` = token in this `/init` body.
pub fn validate_init_access_token(existing: Option<&str>, request: Option<&str>, mmds: MmdsCheck) -> Result<(), InitError> {
    // Fast path: request token equals the existing one.
    if let (Some(e), Some(r)) = (existing, request)
        && bool::from(e.as_bytes().ct_eq(r.as_bytes()))
    {
        return Ok(());
    }
    if mmds.matches {
        return Ok(());
    }
    if existing.is_none() && !mmds.exists {
        return Ok(()); // first-time setup
    }
    if request.is_none() {
        return Err(InitError::ResetNotAuthorized);
    }
    Err(InitError::Mismatch)
}

/// Upstream `checkMMDSHash`. With `-isnotfc` there is no MMDS at all.
async fn check_mmds_hash(state: &AppState, request: Option<&str>) -> MmdsCheck {
    let none = MmdsCheck { matches: false, exists: false };
    if state.config.is_not_fc {
        return none;
    }
    let Ok(opts) = state.mmds.opts().await else { return none };
    if opts.access_token_hash.is_empty() {
        return none;
    }
    let expected = host::hash_access_token(request.unwrap_or(""));
    MmdsCheck { matches: expected == opts.access_token_hash, exists: true }
}

pub async fn post_init(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let version = [("X-Envd-Version", env!("CARGO_PKG_VERSION"))];

    let req: InitRequest = if body.is_empty() {
        InitRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to decode request: {e}");
                return (StatusCode::BAD_REQUEST, version).into_response();
            }
        }
    };
    let request_token = req.access_token.as_deref().filter(|t| !t.is_empty());

    let _guard = state.init_lock.lock().await;

    let mmds = check_mmds_hash(&state, request_token).await;
    if let Err(e) = validate_init_access_token(state.access_token().as_deref(), request_token, mmds) {
        tracing::error!("{}", e.message());
        return (StatusCode::UNAUTHORIZED, version, e.message()).into_response();
    }

    let newer = match req.timestamp {
        None => true,
        Some(ts) => {
            let nanos = ts.timestamp_nanos_opt().unwrap_or(i64::MAX);
            state.last_set_time.fetch_max(nanos, Ordering::SeqCst) < nanos
        }
    };
    if newer {
        set_data(&state, req);
    }
    state.initialized.store(true, Ordering::SeqCst);

    let poll_state = state.clone();
    tokio::spawn(async move {
        host::mmds::poll_for_opts(poll_state.mmds.as_ref(), &poll_state.env_vars).await;
    });

    (StatusCode::NO_CONTENT, version, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

/// Upstream `SetData`.
fn set_data(state: &AppState, req: InitRequest) {
    if let Some(ts) = req.timestamp
        && !state.config.is_not_fc
    {
        host::sync_clock(ts);
    }
    if let Some(vars) = &req.env_vars {
        state.env_vars.replace_user_vars(vars);
    }
    {
        let mut token = state.access_token.write().expect("access token lock poisoned");
        *token = req.access_token.filter(|t| !t.is_empty());
    }
    if let Some(ip) = req.hyperloop_ip {
        std::thread::spawn(move || {
            if let Err(e) = host::hosts::rewrite_hosts_file(&ip, std::path::Path::new("/etc/hosts")) {
                tracing::error!(error = %e, "failed to rewrite /etc/hosts for hyperloop");
            }
        });
    }
    {
        let mut defaults = state.defaults.write().expect("defaults lock poisoned");
        if let Some(user) = req.default_user.filter(|u| !u.is_empty()) {
            defaults.user = user;
        }
        if let Some(workdir) = req.default_workdir.filter(|w| !w.is_empty()) {
            defaults.workdir = Some(workdir);
        }
    }
    if req.ca_bundle.is_some_and(|c| !c.is_empty()) {
        tracing::warn!("caBundle is not supported in phase 1, ignoring");
    }
    if req.volume_mounts.is_some() {
        tracing::warn!("volumeMounts are not supported in phase 1, ignoring");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_MMDS: MmdsCheck = MmdsCheck { matches: false, exists: false };
    const MMDS_MATCH: MmdsCheck = MmdsCheck { matches: true, exists: true };
    const MMDS_OTHER: MmdsCheck = MmdsCheck { matches: false, exists: true };

    #[test]
    fn token_rules() {
        // same token again
        assert_eq!(validate_init_access_token(Some("t1"), Some("t1"), NO_MMDS), Ok(()));
        // orchestrator rotated the token via MMDS
        assert_eq!(validate_init_access_token(Some("t1"), Some("t2"), MMDS_MATCH), Ok(()));
        // first-time setup: no token yet, no MMDS
        assert_eq!(validate_init_access_token(None, Some("t1"), NO_MMDS), Ok(()));
        assert_eq!(validate_init_access_token(None, None, NO_MMDS), Ok(()));
        // trying to clear an existing token without MMDS saying so
        assert_eq!(validate_init_access_token(Some("t1"), None, NO_MMDS), Err(InitError::ResetNotAuthorized));
        // different token, MMDS absent or says something else
        assert_eq!(validate_init_access_token(Some("t1"), Some("t2"), NO_MMDS), Err(InitError::Mismatch));
        assert_eq!(validate_init_access_token(None, Some("t2"), MMDS_OTHER), Err(InitError::Mismatch));
    }
}
