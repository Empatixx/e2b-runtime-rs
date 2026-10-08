use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

const BROKEN_USER_AGENT: &str = "connect-python";
const NOTIFY_HEADER: &str = "X-E2B-Legacy-SDK";

#[derive(Debug, Clone, Copy)]
pub struct LegacySdk;

pub async fn legacy_sdk(mut request: Request, next: Next) -> Response {
    let legacy = request
        .headers()
        .get(header::USER_AGENT)
        .is_some_and(|agent| agent == BROKEN_USER_AGENT);
    if !legacy {
        return next.run(request).await;
    }
    let streaming = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|kind| kind.to_str().ok())
        .is_some_and(|kind| kind.starts_with("application/connect+") || kind.starts_with("application/grpc"));
    request.extensions_mut().insert(LegacySdk);
    let mut response = next.run(request).await;
    if streaming || response.status() == StatusCode::OK {
        response
            .headers_mut()
            .insert(NOTIFY_HEADER, HeaderValue::from_static("true"));
    }
    response
}
