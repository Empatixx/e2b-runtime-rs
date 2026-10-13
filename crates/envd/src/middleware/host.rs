use axum::extract::Request;
use axum::http::{Method, StatusCode, Version, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::connection::go_status_error;

const VALID_HOST_PUNCTUATION: &[u8] = b"!$%&()*+,-.:;=[']_~";

pub async fn require_host(request: Request, next: Next) -> Response {
    match host_problem(&request) {
        Some(problem) => go_status_error(StatusCode::BAD_REQUEST, problem),
        None => next.run(request).await,
    }
}

fn host_problem(request: &Request) -> Option<&'static str> {
    let hosts: Vec<_> = request.headers().get_all(header::HOST).iter().collect();
    let http1 = matches!(request.version(), Version::HTTP_10 | Version::HTTP_11);
    match hosts.as_slice() {
        [] if request.version() == Version::HTTP_11 && request.method() != Method::CONNECT => {
            Some("missing required Host header")
        }
        [host] if !is_valid_host(host.as_bytes()) => Some("malformed Host header"),
        _ => None,
    }
    .filter(|_| http1)
}

fn is_valid_host(host: &[u8]) -> bool {
    host.iter()
        .all(|byte| byte.is_ascii_alphanumeric() || VALID_HOST_PUNCTUATION.contains(byte))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;

    use super::*;

    fn request(version: Version, method: Method, hosts: &[&[u8]]) -> Request {
        hosts
            .iter()
            .fold(
                Request::builder().version(version).method(method).uri("/"),
                |builder, host| builder.header(header::HOST, *host),
            )
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn http_1_1_needs_a_host_like_go() {
        assert_eq!(
            host_problem(&request(Version::HTTP_11, Method::GET, &[])),
            Some("missing required Host header")
        );
        assert_eq!(host_problem(&request(Version::HTTP_11, Method::CONNECT, &[])), None);
        assert_eq!(host_problem(&request(Version::HTTP_10, Method::GET, &[])), None);
        assert_eq!(host_problem(&request(Version::HTTP_2, Method::GET, &[])), None);
        assert_eq!(host_problem(&request(Version::HTTP_11, Method::GET, &[b""])), None);
    }

    #[test]
    fn a_single_host_is_checked_like_httpguts() {
        assert_eq!(
            host_problem(&request(Version::HTTP_11, Method::GET, &[b"[::1]:49983"])),
            None
        );
        assert_eq!(
            host_problem(&request(Version::HTTP_10, Method::GET, &[b"a b"])),
            Some("malformed Host header")
        );
        assert_eq!(
            host_problem(&request(Version::HTTP_11, Method::GET, &[b"a\"b"])),
            Some("malformed Host header")
        );
        assert_eq!(
            host_problem(&request(Version::HTTP_11, Method::GET, &[b"a b", b"c"])),
            None
        );
    }
}
