use std::convert::Infallible;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

const CHUNKING_THRESHOLD: usize = 2048;

pub async fn frame_unary(request: Request, next: Next) -> Response {
    let unary = request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|kind| is_unary(kind.as_bytes()));
    let response = next.run(request).await;
    match unary {
        true => reframed(response).await,
        false => response,
    }
}

fn is_unary(content_type: &[u8]) -> bool {
    !content_type.starts_with(b"application/connect+") && !content_type.starts_with(b"application/grpc")
}

async fn reframed(response: Response) -> Response {
    let (parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, usize::MAX).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let json_error = parts.status != StatusCode::OK
        && parts
            .headers
            .get(header::CONTENT_TYPE)
            .is_some_and(|kind| kind.as_bytes().starts_with(b"application/json"));
    let bytes = match json_error {
        true => Bytes::from(escape_html(&String::from_utf8_lossy(&bytes))),
        false => bytes,
    };
    Response::from_parts(parts, sized_like_go(bytes))
}

fn sized_like_go(bytes: Bytes) -> Body {
    match bytes.len() <= CHUNKING_THRESHOLD {
        true => Body::from(bytes),
        false => Body::from_stream(futures::stream::once(async move { Ok::<_, Infallible>(bytes) })),
    }
}

fn escape_html(json: &str) -> String {
    json.chars()
        .map(|c| match c {
            '<' => "\\u003c".to_owned(),
            '>' => "\\u003e".to_owned(),
            '&' => "\\u0026".to_owned(),
            '\u{2028}' => "\\u2028".to_owned(),
            '\u{2029}' => "\\u2029".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_json_is_escaped_like_go_encoding_json() {
        assert_eq!(
            escape_html(r#"{"message":"a <b> & c"}"#),
            r#"{"message":"a \u003cb\u003e \u0026 c"}"#
        );
    }

    #[test]
    fn only_connect_unary_bodies_are_reframed() {
        assert!(is_unary(b"application/json"));
        assert!(is_unary(b"application/proto"));
        assert!(!is_unary(b"application/connect+json"));
        assert!(!is_unary(b"application/grpc-web+proto"));
    }
}
