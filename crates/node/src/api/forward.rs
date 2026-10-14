use std::collections::HashMap;

use axum::body::{Body, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderName, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::ApiState;
use super::error::ApiError;
use crate::sandbox::Location;

const FORWARDED: HeaderName = HeaderName::from_static("x-e2b-simple-forwarded");
const MAX_BODY: usize = 16 * 1024 * 1024;

pub async fn to_owner(
    State(state): State<ApiState>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(sandbox_id) = params.get("id").filter(|_| !request.headers().contains_key(FORWARDED)) else {
        return next.run(request).await;
    };
    match state.sandboxes.locate(sandbox_id).await {
        Ok(Location::Elsewhere(owner)) => forward(&state.http, &owner.api_url, request)
            .await
            .unwrap_or_else(|e| e.into_response()),
        Ok(_) => next.run(request).await,
        Err(e) => ApiError::from(e).into_response(),
    }
}

async fn forward(http: &reqwest::Client, api_url: &str, request: Request) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
    let body = to_bytes(body, MAX_BODY).await.map_err(bad_gateway)?;
    let mut headers = parts.headers;
    headers.remove("host");
    headers.insert(FORWARDED, "1".parse().expect("static header value"));
    let answer = http
        .request(parts.method, format!("{api_url}{path}"))
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(bad_gateway)?;
    let mut response = Response::builder().status(answer.status());
    for (name, value) in answer.headers() {
        response = response.header(name, value);
    }
    let body = answer.bytes().await.map_err(bad_gateway)?;
    response.body(Body::from(body)).map_err(bad_gateway)
}

fn bad_gateway(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, format!("forward to owner node: {error}"))
}
