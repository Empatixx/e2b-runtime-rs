use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use super::ApiState;
use super::error::{ApiError, ApiResult};
use super::models::{NewVolume, VolumeModel};

pub async fn create(
    State(state): State<ApiState>,
    Json(body): Json<NewVolume>,
) -> ApiResult<(StatusCode, Json<VolumeModel>)> {
    if state.host.volumes.get(&body.name).is_some() {
        return Err(ApiError::conflict(format!("volume {} already exists", body.name)));
    }
    let volume = state.host.volumes.create(&body.name).await?;
    Ok((StatusCode::CREATED, Json(VolumeModel::new(&volume, true))))
}

pub async fn list(State(state): State<ApiState>) -> Json<Vec<VolumeModel>> {
    Json(
        state
            .host
            .volumes
            .list()
            .iter()
            .map(|volume| VolumeModel::new(volume, false))
            .collect(),
    )
}

pub async fn detail(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<Json<VolumeModel>> {
    let volume = state
        .host
        .volumes
        .get(&id)
        .ok_or_else(|| ApiError::not_found("volume", &id))?;
    Ok(Json(VolumeModel::new(&volume, true)))
}

pub async fn delete(State(state): State<ApiState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    match state.host.volumes.delete(&id).await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(ApiError::not_found("volume", &id)),
    }
}
