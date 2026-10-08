use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const ALLOWED_METHODS: [Method; 7] = [
    Method::HEAD,
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
    Method::OPTIONS,
];
const EXPOSED_HEADERS: &str =
    "Grpc-Status, Grpc-Message, Grpc-Status-Details-Bin, Location, Cache-Control, X-Content-Type-Options";
const MAX_AGE: &str = "7200";

pub async fn cors(request: Request, next: Next) -> Response {
    let preflight =
        request.method() == Method::OPTIONS && request.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    match preflight {
        true => preflight_response(request.headers()),
        false => {
            let allowed = has_origin(request.headers()) && method_allowed(request.method().as_str());
            let mut response = next.run(request).await;
            let headers = response.headers_mut();
            headers.append(header::VARY, HeaderValue::from_static("Origin"));
            if allowed {
                headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
                headers.insert(
                    header::ACCESS_CONTROL_EXPOSE_HEADERS,
                    HeaderValue::from_static(EXPOSED_HEADERS),
                );
            }
            response
        }
    }
}

fn preflight_response(request: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    [
        "Origin",
        "Access-Control-Request-Method",
        "Access-Control-Request-Headers",
    ]
    .into_iter()
    .for_each(|vary| {
        headers.append(header::VARY, HeaderValue::from_static(vary));
    });
    let requested_method = request
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|method| method.to_str().ok())
        .unwrap_or_default()
        .to_uppercase();
    if !has_origin(request) || !method_allowed(&requested_method) {
        return response;
    }
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    if let Ok(method) = HeaderValue::from_str(&requested_method) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, method);
    }
    if let Some(requested_headers) = request.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested_headers.clone());
    }
    headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(MAX_AGE));
    response
}

fn has_origin(headers: &HeaderMap) -> bool {
    headers.get(header::ORIGIN).is_some_and(|origin| !origin.is_empty())
}

fn method_allowed(method: &str) -> bool {
    ALLOWED_METHODS
        .iter()
        .any(|allowed| allowed.as_str().eq_ignore_ascii_case(method))
}
