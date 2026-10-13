use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

const SUPPORTED: HeaderValue = HeaderValue::from_static("gzip");
const CONNECT_ACCEPT_ENCODING: HeaderName = HeaderName::from_static("connect-accept-encoding");

pub async fn advertise_encodings(request: Request, next: Next) -> Response {
    let advertised = request
        .headers()
        .get(header::CONTENT_TYPE)
        .map_or(Some(header::ACCEPT_ENCODING), |kind| advertised_by(kind.as_bytes()));
    let mut response = next.run(request).await;
    let speaks_connect = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|kind| kind.as_bytes().starts_with(b"application/"));
    if let Some(name) = advertised.filter(|_| speaks_connect) {
        response.headers_mut().entry(name).or_insert(SUPPORTED);
    }
    response
}

fn advertised_by(content_type: &[u8]) -> Option<HeaderName> {
    match content_type {
        kind if kind.starts_with(b"application/grpc") => None,
        kind if kind.starts_with(b"application/connect+") => Some(CONNECT_ACCEPT_ENCODING),
        _ => Some(header::ACCEPT_ENCODING),
    }
}
