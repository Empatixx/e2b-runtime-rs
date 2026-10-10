use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, CONTENT_TYPE, HeaderMap, HeaderValue,
};
use axum::http::{Method, StatusCode};
use bytes::Bytes;
use hyper::Response;

use super::body::{ProxyBody, empty_body, full_body};

const PREFLIGHT_MAX_AGE: &str = "86400";

pub fn set_headers(headers: &mut HeaderMap) {
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
}

pub fn error(message: &str, status: StatusCode) -> Response<ProxyBody> {
    let mut response = text_response(status, format!("{message}\n"));
    set_headers(response.headers_mut());
    response
}

pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers
            .get(ACCESS_CONTROL_REQUEST_METHOD)
            .is_some_and(|value| !value.is_empty())
}

pub fn handle_preflight(method: &Method, headers: &HeaderMap) -> Option<Response<ProxyBody>> {
    is_preflight(method, headers).then(|| preflight_response(headers))
}

fn preflight_response(request_headers: &HeaderMap) -> Response<ProxyBody> {
    let mut response = Response::new(empty_body());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let headers = response.headers_mut();
    set_headers(headers);
    headers.insert(ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("*"));
    request_headers
        .get(ACCESS_CONTROL_REQUEST_HEADERS)
        .filter(|requested| !requested.is_empty())
        .into_iter()
        .for_each(|requested| {
            headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
        });
    headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(PREFLIGHT_MAX_AGE));
    response
}

pub fn text_response(status: StatusCode, body: String) -> Response<ProxyBody> {
    let mut response = Response::new(full_body(Bytes::from(body)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    response
}

#[cfg(test)]
mod tests {
    use axum::http::header::HeaderName;

    use super::super::body::read_text;
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn handle_preflight_answers_preflight() {
        let request = headers(&[
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-access-token, content-type"),
        ]);
        let response = handle_preflight(&Method::OPTIONS, &request).unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(response.headers()["access-control-allow-methods"], "*");
        assert_eq!(
            response.headers()["access-control-allow-headers"],
            "x-access-token, content-type"
        );
        assert_eq!(response.headers()["access-control-max-age"], "86400");
    }

    #[test]
    fn handle_preflight_omits_allow_headers_when_none_requested() {
        let request = headers(&[("access-control-request-method", "GET")]);
        let response = handle_preflight(&Method::OPTIONS, &request).unwrap();
        assert!(!response.headers().contains_key("access-control-allow-headers"));
    }

    #[test]
    fn handle_preflight_ignores_non_preflights() {
        let with_request_method = headers(&[("access-control-request-method", "GET")]);
        assert!(handle_preflight(&Method::GET, &HeaderMap::new()).is_none());
        assert!(handle_preflight(&Method::GET, &with_request_method).is_none());
        assert!(handle_preflight(&Method::OPTIONS, &HeaderMap::new()).is_none());
    }

    #[tokio::test]
    async fn error_writes_status_body_and_header() {
        let response = error("sandbox unreachable", StatusCode::BAD_GATEWAY);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(response.headers()["content-type"], "text/plain; charset=utf-8");
        assert_eq!(read_text(response).await, "sandbox unreachable\n");
    }
}
