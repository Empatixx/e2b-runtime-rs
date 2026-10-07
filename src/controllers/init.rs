use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::app::AppState;
use crate::host;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitRequest {
    pub env_vars: Option<BTreeMap<String, String>>,
    pub access_token: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub default_user: Option<String>,
    pub default_workdir: Option<String>,
    #[serde(rename = "hyperloopIP")]
    pub hyperloop_ip: Option<String>,
    pub ca_bundle: Option<String>,
    pub volume_mounts: Option<serde_json::Value>,
    #[serde(rename = "lifecycleID")]
    pub lifecycle_id: Option<String>,
}

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
    let version = [("X-Envd-Version", env!("CARGO_PKG_VERSION"))];

    let request = if body.is_empty() {
        InitRequest::default()
    } else {
        match serde_json::from_slice::<InitRequest>(&body) {
            Ok(request) => request,
            Err(e) => {
                tracing::error!("Failed to decode request: {e}");
                return (StatusCode::BAD_REQUEST, version).into_response();
            }
        }
    };
    let requested_token = request.access_token.as_deref().filter(|t| !t.is_empty());

    let _guard = state.init_lock.lock().await;

    let mmds_hash = if state.config.is_not_fc {
        None
    } else {
        host::mmds::get_opts()
            .await
            .ok()
            .map(|opts| opts.access_token_hash)
            .filter(|hash| !hash.is_empty())
    };
    if let Err(e) = validate_token(state.access_token().as_deref(), requested_token, mmds_hash.as_deref()) {
        tracing::error!("{e}");
        return (StatusCode::UNAUTHORIZED, version, e).into_response();
    }

    if is_newest(&state, request.timestamp) {
        apply(&state, request);
    }
    state.initialized.store(true, Ordering::SeqCst);

    let state = state.clone();
    tokio::spawn(async move { host::mmds::poll_for_opts(&state.env_vars).await });

    (StatusCode::NO_CONTENT, version, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

fn is_newest(state: &AppState, timestamp: Option<DateTime<Utc>>) -> bool {
    timestamp
        .map(|timestamp| timestamp.timestamp_nanos_opt().unwrap_or(i64::MAX))
        .is_none_or(|nanos| state.last_set_time.fetch_max(nanos, Ordering::SeqCst) < nanos)
}

fn apply(state: &AppState, request: InitRequest) {
    if let Some(timestamp) = request.timestamp.filter(|_| !state.config.is_not_fc) {
        host::sync_clock(timestamp);
    }

    if let Some(vars) = &request.env_vars {
        state.env_vars.replace_user_vars(vars);
    }

    *state.access_token.write().unwrap_or_else(PoisonError::into_inner) =
        request.access_token.filter(|t| !t.is_empty());

    if let Some(ip) = request.hyperloop_ip {
        std::thread::spawn(move || {
            if let Err(e) = host::hosts::rewrite_hosts_file(&ip, Path::new("/etc/hosts")) {
                tracing::error!(error = %e, "failed to rewrite /etc/hosts for hyperloop");
            }
        });
    }

    {
        let mut defaults = state.defaults.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(user) = request.default_user.filter(|u| !u.is_empty()) {
            defaults.user = user;
        }
        if let Some(workdir) = request.default_workdir.filter(|w| !w.is_empty()) {
            defaults.workdir = Some(workdir);
        }
    }

    if request.ca_bundle.is_some_and(|c| !c.is_empty()) {
        tracing::warn!("caBundle is not supported yet, ignoring");
    }
    if request.volume_mounts.is_some() {
        tracing::warn!("volumeMounts are not supported yet, ignoring");
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
