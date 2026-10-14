use std::time::Duration;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;

use super::ApiState;
use super::error::{ApiError, ApiResult};
use super::models::{NewSandbox, NewSnapshot, Refresh, SandboxDetailModel, SandboxModel, SnapshotModel, Timeout};
use crate::sandbox::CreateRequest;

const KNOWN_STATES: [&str; 2] = ["running", "paused"];

pub async fn create(
    State(state): State<ApiState>,
    Json(body): Json<NewSandbox>,
) -> ApiResult<(StatusCode, Json<SandboxModel>)> {
    let request = CreateRequest {
        template_id: body.template_id,
        timeout: Duration::from_secs(body.timeout),
        auto_pause: body.auto_pause,
        metadata: body.metadata,
        env_vars: body.env_vars,
        volume_mounts: body.volume_mounts.into_iter().map(Into::into).collect(),
    };
    let info = state.sandboxes.create(request).await?;
    Ok((StatusCode::CREATED, Json(SandboxModel::new(&info, &state.domain))))
}

pub async fn list_running(State(state): State<ApiState>) -> Json<Vec<SandboxDetailModel>> {
    let running = state.sandboxes.list().await.into_iter().filter(|info| !info.paused);
    Json(
        running
            .map(|info| SandboxDetailModel::new(&info, &state.domain))
            .collect(),
    )
}

pub async fn list(State(state): State<ApiState>, RawQuery(query): RawQuery) -> Json<Vec<SandboxDetailModel>> {
    let wanted = requested_states(query.as_deref());
    let sandboxes = state
        .sandboxes
        .list()
        .await
        .into_iter()
        .map(|info| SandboxDetailModel::new(&info, &state.domain));
    Json(sandboxes.filter(|sandbox| wanted.contains(&sandbox.state)).collect())
}

pub async fn detail(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<SandboxDetailModel>> {
    let info = state
        .sandboxes
        .get(&id)
        .await
        .ok_or_else(|| ApiError::not_found("sandbox", &id))?;
    Ok(Json(SandboxDetailModel::new(&info, &state.domain)))
}

pub async fn kill(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    match state.sandboxes.kill(&id).await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(ApiError::not_found("sandbox", &id)),
    }
}

pub async fn pause(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let info = state
        .sandboxes
        .get(&id)
        .await
        .ok_or_else(|| ApiError::not_found("sandbox", &id))?;
    if info.paused {
        return Err(ApiError::conflict(format!("sandbox {id} is already paused")));
    }
    state.sandboxes.pause(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resume(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Option<Json<Timeout>>,
) -> ApiResult<(StatusCode, Json<SandboxModel>)> {
    let info = state
        .sandboxes
        .get(&id)
        .await
        .ok_or_else(|| ApiError::not_found("sandbox", &id))?;
    if !info.paused {
        return Err(ApiError::conflict(format!("sandbox {id} is already running")));
    }
    let timeout = body.map_or(15, |Json(body)| body.timeout);
    let (info, _) = state.sandboxes.connect(&id, Duration::from_secs(timeout)).await?;
    Ok((StatusCode::CREATED, Json(SandboxModel::new(&info, &state.domain))))
}

pub async fn connect(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Timeout>,
) -> ApiResult<(StatusCode, Json<SandboxModel>)> {
    state
        .sandboxes
        .get(&id)
        .await
        .ok_or_else(|| ApiError::not_found("sandbox", &id))?;
    let (info, resumed) = state.sandboxes.connect(&id, Duration::from_secs(body.timeout)).await?;
    let status = if resumed { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, Json(SandboxModel::new(&info, &state.domain))))
}

pub async fn set_timeout(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<Timeout>,
) -> ApiResult<StatusCode> {
    state
        .sandboxes
        .get(&id)
        .await
        .ok_or_else(|| ApiError::not_found("sandbox", &id))?;
    state
        .sandboxes
        .set_timeout(&id, Duration::from_secs(body.timeout))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn refresh(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Option<Json<Refresh>>,
) -> ApiResult<StatusCode> {
    state
        .sandboxes
        .get(&id)
        .await
        .ok_or_else(|| ApiError::not_found("sandbox", &id))?;
    let seconds = body.and_then(|Json(body)| body.duration).unwrap_or(60);
    state.sandboxes.set_timeout(&id, Duration::from_secs(seconds)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn snapshot(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Option<Json<NewSnapshot>>,
) -> ApiResult<(StatusCode, Json<SnapshotModel>)> {
    state
        .sandboxes
        .get(&id)
        .await
        .ok_or_else(|| ApiError::not_found("sandbox", &id))?;
    let name = body.and_then(|Json(body)| body.name);
    let template_id = state.sandboxes.snapshot(&id, name).await?;
    let snapshot = SnapshotModel {
        snapshot_id: format!("{template_id}:default"),
        names: vec![format!("{template_id}:default")],
    };
    Ok((StatusCode::CREATED, Json(snapshot)))
}

fn requested_states(query: Option<&str>) -> Vec<&'static str> {
    let query = query.unwrap_or_default().replace("%2C", ",").replace("%2c", ",");
    let requested: Vec<&'static str> = query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("state="))
        .flat_map(|states| states.split(','))
        .filter_map(known_state)
        .collect();
    match requested.is_empty() {
        true => KNOWN_STATES.to_vec(),
        false => requested,
    }
}

fn known_state(state: &str) -> Option<&'static str> {
    KNOWN_STATES.into_iter().find(|known| *known == state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_state_filters_from_query() {
        assert_eq!(requested_states(Some("state=running&limit=5")), vec!["running"]);
        assert_eq!(
            requested_states(Some("state=running%2Cpaused")),
            vec!["running", "paused"]
        );
        assert_eq!(requested_states(None), vec!["running", "paused"]);
    }
}
