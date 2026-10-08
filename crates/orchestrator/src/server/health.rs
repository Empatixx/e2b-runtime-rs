use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use tonic::service::Routes;

use super::status::NodeStatus;

#[derive(Clone)]
struct Health {
    status: Arc<NodeStatus>,
    commit: String,
}

pub fn with_health(routes: Routes, status: Arc<NodeStatus>, commit: &str) -> Router {
    routes.into_axum_router().route(
        "/health",
        get(health).with_state(Health {
            status,
            commit: commit.to_owned(),
        }),
    )
}

async fn health(State(state): State<Health>) -> Response {
    let health = state.status.health();
    let body = format!("{}\n", json!({ "status": health.status, "version": state.commit }));
    (
        StatusCode::from_u16(health.code).unwrap_or(StatusCode::OK),
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::proto::ServiceInfoStatus;

    async fn get_health(status: Arc<NodeStatus>) -> (u16, String, String) {
        let response = with_health(Routes::default(), status, "abc123")
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let code = response.status().as_u16();
        let content_type = response.headers()[header::CONTENT_TYPE].to_str().unwrap().to_owned();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (code, content_type, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn health_reports_the_status_and_the_commit() {
        let status = Arc::new(NodeStatus::default());
        assert_eq!(
            get_health(status.clone()).await,
            (
                200,
                "application/json".to_owned(),
                "{\"status\":\"healthy\",\"version\":\"abc123\"}\n".to_owned()
            )
        );

        status.override_to(ServiceInfoStatus::Draining).unwrap();
        assert_eq!(
            get_health(status.clone()).await.2,
            "{\"status\":\"draining\",\"version\":\"abc123\"}\n"
        );

        status.override_to(ServiceInfoStatus::Unhealthy).unwrap();
        assert_eq!(get_health(status).await.0, 503);
    }
}
