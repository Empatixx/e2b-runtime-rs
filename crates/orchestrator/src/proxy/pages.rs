pub mod template;

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::http::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use axum::http::{Response, StatusCode};
use bytes::Bytes;
use serde::Serialize;

use super::body::{ProxyBody, full_body};
use super::cors;
use template::Template;

const LAYOUT: &str = include_str!("../../assets/proxy/browser_layout.html");
const BROWSER_USER_AGENTS: [&str; 7] = ["mozilla", "chrome", "safari", "firefox", "edge", "opera", "msie"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageKind {
    SandboxNotFound,
    PortClosed,
    StillTransitioning,
    ResumePermissionDenied,
    TooManyConnections,
    TeamSandboxLimit,
    InternalRoute,
    MissingTrafficAccessToken,
    InvalidTrafficAccessToken,
}

impl PageKind {
    const ALL: [PageKind; 9] = [
        PageKind::SandboxNotFound,
        PageKind::PortClosed,
        PageKind::StillTransitioning,
        PageKind::ResumePermissionDenied,
        PageKind::TooManyConnections,
        PageKind::TeamSandboxLimit,
        PageKind::InternalRoute,
        PageKind::MissingTrafficAccessToken,
        PageKind::InvalidTrafficAccessToken,
    ];

    fn source(self) -> &'static str {
        match self {
            PageKind::SandboxNotFound => include_str!("../../assets/proxy/browser_sandbox_not_found.html"),
            PageKind::PortClosed => include_str!("../../assets/proxy/browser_port_closed.html"),
            PageKind::StillTransitioning => {
                include_str!("../../assets/proxy/browser_sandbox_still_transitioning.html")
            }
            PageKind::ResumePermissionDenied => {
                include_str!("../../assets/proxy/browser_sandbox_resume_permission_denied.html")
            }
            PageKind::TooManyConnections => {
                include_str!("../../assets/proxy/browser_sandbox_too_many_connections.html")
            }
            PageKind::TeamSandboxLimit => include_str!("../../assets/proxy/browser_team_sandbox_limit.html"),
            PageKind::InternalRoute => include_str!("../../assets/proxy/browser_internal_route.html"),
            PageKind::MissingTrafficAccessToken => {
                include_str!("../../assets/proxy/browser_traffic_access_token_missing_error.html")
            }
            PageKind::InvalidTrafficAccessToken => {
                include_str!("../../assets/proxy/browser_traffic_access_token_invalid_error.html")
            }
        }
    }
}

static TEMPLATES: LazyLock<HashMap<PageKind, Template>> = LazyLock::new(|| {
    PageKind::ALL
        .into_iter()
        .map(|kind| (kind, Template::parse(LAYOUT, kind.source())))
        .collect()
});

#[derive(Debug, Clone, Serialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ErrorPage {
    #[serde(skip)]
    kind: PageKind,
    #[serde(rename = "sandboxId", skip_serializing_if = "Option::is_none")]
    sandbox_id: Option<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u64>,
    code: u16,
    #[serde(rename = "connectionLimit", skip_serializing_if = "Option::is_none")]
    connection_limit: Option<i64>,
    #[serde(skip)]
    host: String,
    #[serde(skip)]
    header: Option<String>,
}

impl ErrorPage {
    pub fn sandbox_not_found(sandbox_id: &str, host: &str) -> Self {
        Self::builder()
            .kind(PageKind::SandboxNotFound)
            .sandbox_id(sandbox_id)
            .message("The sandbox was not found")
            .code(StatusCode::BAD_GATEWAY.as_u16())
            .host(host)
            .build()
    }

    pub fn port_closed(sandbox_id: &str, host: &str, port: u64) -> Self {
        Self::builder()
            .kind(PageKind::PortClosed)
            .sandbox_id(sandbox_id)
            .message("The sandbox is running but port is not open")
            .port(port)
            .code(StatusCode::BAD_GATEWAY.as_u16())
            .host(host)
            .build()
    }

    pub fn still_transitioning(sandbox_id: &str, host: &str) -> Self {
        Self::builder()
            .kind(PageKind::StillTransitioning)
            .sandbox_id(sandbox_id)
            .message("The sandbox is still transitioning. Try again in a moment.")
            .code(StatusCode::CONFLICT.as_u16())
            .host(host)
            .build()
    }

    pub fn resume_permission_denied(sandbox_id: &str, host: &str) -> Self {
        Self::builder()
            .kind(PageKind::ResumePermissionDenied)
            .sandbox_id(sandbox_id)
            .message(
                "This sandbox can't be resumed with the credentials provided. Check your access token and try again.",
            )
            .code(StatusCode::FORBIDDEN.as_u16())
            .host(host)
            .build()
    }

    pub fn too_many_connections(sandbox_id: &str, host: &str, connection_limit: i64) -> Self {
        Self::builder()
            .kind(PageKind::TooManyConnections)
            .sandbox_id(sandbox_id)
            .message(format!(
                "The sandbox has too many concurrent incoming connections (limit: {connection_limit})"
            ))
            .code(StatusCode::TOO_MANY_REQUESTS.as_u16())
            .connection_limit(connection_limit)
            .host(host)
            .build()
    }

    pub fn team_sandbox_limit(sandbox_id: &str, host: &str, message: &str) -> Self {
        Self::builder()
            .kind(PageKind::TeamSandboxLimit)
            .sandbox_id(sandbox_id)
            .message(
                Some(message)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Sandbox limit reached"),
            )
            .code(StatusCode::TOO_MANY_REQUESTS.as_u16())
            .host(host)
            .build()
    }

    pub fn internal_route(host: &str, path: &str) -> Self {
        Self::builder()
            .kind(PageKind::InternalRoute)
            .message("This endpoint is reserved for the E2B control plane and is not reachable through the sandbox URL")
            .path(path)
            .code(StatusCode::NOT_FOUND.as_u16())
            .host(host)
            .build()
    }

    pub fn missing_traffic_access_token(sandbox_id: &str, host: &str, header: &str) -> Self {
        Self::builder()
            .kind(PageKind::MissingTrafficAccessToken)
            .sandbox_id(sandbox_id)
            .message(format!(
                "Sandbox is secured with traffic access token. Token header '{header}' is missing"
            ))
            .code(StatusCode::FORBIDDEN.as_u16())
            .host(host)
            .header(header)
            .build()
    }

    pub fn invalid_traffic_access_token(sandbox_id: &str, host: &str, header: &str) -> Self {
        Self::builder()
            .kind(PageKind::InvalidTrafficAccessToken)
            .sandbox_id(sandbox_id)
            .message(format!(
                "Sandbox is secured with traffic access token. Provided token in header '{header}' is invalid"
            ))
            .code(StatusCode::FORBIDDEN.as_u16())
            .host(host)
            .header(header)
            .build()
    }

    pub fn respond(&self, request_headers: &HeaderMap) -> Result<Response<ProxyBody>, String> {
        let status = StatusCode::from_u16(self.code)
            .ok()
            .filter(|_| self.code > 0)
            .ok_or_else(|| format!("invalid status code: {}", self.code))?;
        let (content_type, body) = match wants_html(request_headers) {
            true => ("text/html; charset=utf-8", self.html()),
            false => ("application/json; charset=utf-8", self.json()?),
        };
        let mut response = Response::new(full_body(Bytes::from(body)));
        *response.status_mut() = status;
        cors::set_headers(response.headers_mut());
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        Ok(response)
    }

    fn json(&self) -> Result<String, String> {
        serde_json::to_string(self)
            .map(|json| escape_json_html(&json))
            .map_err(|e| e.to_string())
    }

    fn html(&self) -> String {
        TEMPLATES[&self.kind].render(&|name| self.field(name))
    }

    fn field(&self, name: &str) -> String {
        match name {
            "SandboxId" => self.sandbox_id.clone().unwrap_or_default(),
            "Message" => self.message.clone(),
            "Code" => self.code.to_string(),
            "Header" => self.header.clone().unwrap_or_default(),
            "Host" => self.host.clone(),
            "Path" => self.path.clone().unwrap_or_default(),
            "Port" => self.port.map(|port| port.to_string()).unwrap_or_default(),
            "ConnectionLimit" => self.connection_limit.map(|limit| limit.to_string()).unwrap_or_default(),
            _ => String::new(),
        }
    }
}

fn escape_json_html(json: &str) -> String {
    json.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn wants_html(headers: &HeaderMap) -> bool {
    !prefers_json(headers) && !is_script_initiated(headers) && is_browser(headers)
}

fn header_lowercase(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> String {
    headers
        .get(name)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).to_lowercase())
        .unwrap_or_default()
}

fn prefers_json(headers: &HeaderMap) -> bool {
    let accept = header_lowercase(headers, ACCEPT);
    accept.contains("application/json") && !accept.contains("text/html")
}

fn is_script_initiated(headers: &HeaderMap) -> bool {
    match header_lowercase(headers, "sec-fetch-mode").as_str() {
        "cors" | "no-cors" | "same-origin" | "websocket" => true,
        "navigate" => false,
        _ => headers.get("x-requested-with").is_some_and(|value| !value.is_empty()),
    }
}

fn is_browser(headers: &HeaderMap) -> bool {
    let user_agent = header_lowercase(headers, USER_AGENT);
    BROWSER_USER_AGENTS.iter().any(|browser| user_agent.contains(browser))
}

#[cfg(test)]
mod tests {
    use super::super::body::read_text;
    use super::*;

    const SANDBOX: &str = "im9r2ycjiy2534qsdy1oo";
    const HOST: &str = "3000-im9r2ycjiy2534qsdy1oo.e2b.app";
    const HOSTILE: &str = "<script>alert(1)</script>";
    const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3";
    const HTML: &str = "text/html; charset=utf-8";
    const JSON: &str = "application/json; charset=utf-8";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    fn content_type_for(pairs: &[(&str, &str)]) -> String {
        let response = ErrorPage::sandbox_not_found(SANDBOX, "e2b.app")
            .respond(&headers(pairs))
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        response.headers()["content-type"].to_str().unwrap().to_owned()
    }

    type NegotiationCase = (&'static str, &'static [(&'static str, &'static str)], &'static str);

    #[test]
    fn content_negotiation_follows_the_request_intent() {
        let cases: [NegotiationCase; 10] = [
            ("no user agent", &[], JSON),
            (
                "top-level navigation",
                &[
                    ("user-agent", CHROME),
                    ("sec-fetch-mode", "navigate"),
                    ("accept", "text/html,application/xhtml+xml,*/*"),
                ],
                HTML,
            ),
            ("bare browser user agent", &[("user-agent", CHROME)], HTML),
            (
                "browser user agent accepting html",
                &[("user-agent", CHROME), ("accept", "text/html")],
                HTML,
            ),
            (
                "browser user agent accepting json",
                &[("user-agent", CHROME), ("accept", "application/json")],
                JSON,
            ),
            (
                "cross-origin fetch",
                &[("user-agent", CHROME), ("sec-fetch-mode", "cors"), ("accept", "*/*")],
                JSON,
            ),
            (
                "same-origin fetch",
                &[
                    ("user-agent", CHROME),
                    ("sec-fetch-mode", "same-origin"),
                    ("accept", "*/*"),
                ],
                JSON,
            ),
            (
                "websocket handshake",
                &[
                    ("user-agent", CHROME),
                    ("sec-fetch-mode", "websocket"),
                    ("accept", "*/*"),
                ],
                JSON,
            ),
            (
                "legacy xhr",
                &[("user-agent", CHROME), ("x-requested-with", "XMLHttpRequest")],
                JSON,
            ),
            (
                "navigation carrying x-requested-with",
                &[
                    ("user-agent", CHROME),
                    ("sec-fetch-mode", "navigate"),
                    ("x-requested-with", "XMLHttpRequest"),
                ],
                HTML,
            ),
        ];
        for (name, request, expected) in cases {
            assert_eq!(content_type_for(request), expected, "{name}");
        }
    }

    fn every_page() -> Vec<(&'static str, ErrorPage)> {
        vec![
            ("sandbox not found", ErrorPage::sandbox_not_found(SANDBOX, "e2b.app")),
            ("port closed", ErrorPage::port_closed(SANDBOX, "e2b.app", 3000)),
            (
                "still transitioning",
                ErrorPage::still_transitioning(SANDBOX, "e2b.app"),
            ),
            (
                "resume permission denied",
                ErrorPage::resume_permission_denied(SANDBOX, "e2b.app"),
            ),
            (
                "too many connections",
                ErrorPage::too_many_connections(SANDBOX, "e2b.app", 100),
            ),
            (
                "team sandbox limit",
                ErrorPage::team_sandbox_limit(SANDBOX, "e2b.app", "limit reached"),
            ),
            ("internal route", ErrorPage::internal_route("e2b.app", "/init")),
            (
                "traffic access token missing",
                ErrorPage::missing_traffic_access_token(SANDBOX, "e2b.app", "e2b-traffic-access-token"),
            ),
            (
                "traffic access token invalid",
                ErrorPage::invalid_traffic_access_token(SANDBOX, "e2b.app", "e2b-traffic-access-token"),
            ),
        ]
    }

    #[test]
    fn every_error_carries_the_cors_header() {
        for (name, page) in every_page() {
            for request in [headers(&[]), headers(&[("user-agent", CHROME)])] {
                let response = page.respond(&request).unwrap();
                assert_eq!(response.headers()["access-control-allow-origin"], "*", "{name}");
            }
        }
    }

    #[test]
    fn rejects_an_invalid_status_code() {
        let page = ErrorPage::builder()
            .kind(PageKind::SandboxNotFound)
            .message("x")
            .code(0)
            .host("")
            .build();
        assert!(page.respond(&headers(&[])).is_err());
    }

    fn html_of(page: &ErrorPage) -> String {
        page.html()
    }

    fn assert_page(page: &ErrorPage, title: &str, status: &str, contains: &[&str], omits: &[&str]) {
        let html = html_of(page);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.trim().ends_with("</html>"));
        assert!(html.contains(&format!("<title>{title} · E2B</title>")));
        assert!(html.contains(&format!("<h1 id=\"title\">{title}</h1>")));
        assert!(html.contains(status), "{status}");
        contains.iter().for_each(|text| assert!(html.contains(text), "{text}"));
        omits.iter().for_each(|text| assert!(!html.contains(text), "{text}"));
    }

    #[test]
    fn browser_pages_render_through_the_layout() {
        assert_page(
            &ErrorPage::sandbox_not_found(SANDBOX, HOST),
            "Sandbox not found",
            "502 Bad Gateway",
            &[SANDBOX],
            &[],
        );
        assert_page(
            &ErrorPage::port_closed(SANDBOX, HOST, 3000),
            "Port closed",
            "502 Bad Gateway",
            &[SANDBOX, HOST, "port 3000"],
            &[],
        );
        assert_page(
            &ErrorPage::still_transitioning(SANDBOX, HOST),
            "Sandbox still transitioning",
            "409 Conflict",
            &[SANDBOX],
            &[],
        );
        assert_page(
            &ErrorPage::resume_permission_denied(SANDBOX, HOST),
            "Unable to resume sandbox",
            "403 Forbidden",
            &[SANDBOX],
            &[],
        );
        assert_page(
            &ErrorPage::too_many_connections(SANDBOX, HOST, 100),
            "Too many connections",
            "429 Too Many Requests",
            &[SANDBOX, "limit: 100"],
            &[],
        );
        assert_page(
            &ErrorPage::team_sandbox_limit(SANDBOX, HOST, HOSTILE),
            "Sandbox limit reached",
            "429 Too Many Requests",
            &["&lt;script&gt;"],
            &[HOSTILE],
        );
        assert_page(
            &ErrorPage::missing_traffic_access_token(SANDBOX, HOST, "e2b-traffic-access-token"),
            "Missing traffic access token",
            "403 Forbidden",
            &[SANDBOX, "e2b-traffic-access-token"],
            &[],
        );
        assert_page(
            &ErrorPage::invalid_traffic_access_token(SANDBOX, HOST, "e2b-traffic-access-token"),
            "Invalid traffic access token",
            "403 Forbidden",
            &[SANDBOX, "e2b-traffic-access-token"],
            &[],
        );
        assert_page(
            &ErrorPage::internal_route(HOST, &format!("/init{HOSTILE}")),
            "Endpoint not available",
            "404 Not Found",
            &["/init&lt;script&gt;"],
            &[HOSTILE, SANDBOX],
        );
    }

    #[tokio::test]
    async fn json_escapes_html_like_go() {
        let page = ErrorPage::internal_route(HOST, "/a<b>&c");
        let body = read_text(page.respond(&headers(&[])).unwrap()).await;
        assert!(body.contains(r#""path":"/a\u003cb\u003e\u0026c""#), "{body}");
    }

    #[tokio::test]
    async fn json_keeps_the_upstream_field_order_and_names() {
        let cases = [
            (
                ErrorPage::sandbox_not_found(SANDBOX, HOST),
                r#"{"sandboxId":"im9r2ycjiy2534qsdy1oo","message":"The sandbox was not found","code":502}"#,
            ),
            (
                ErrorPage::port_closed(SANDBOX, HOST, 3000),
                r#"{"sandboxId":"im9r2ycjiy2534qsdy1oo","message":"The sandbox is running but port is not open","port":3000,"code":502}"#,
            ),
            (
                ErrorPage::too_many_connections(SANDBOX, HOST, 100),
                r#"{"sandboxId":"im9r2ycjiy2534qsdy1oo","message":"The sandbox has too many concurrent incoming connections (limit: 100)","code":429,"connectionLimit":100}"#,
            ),
            (
                ErrorPage::internal_route(HOST, "/init"),
                r#"{"message":"This endpoint is reserved for the E2B control plane and is not reachable through the sandbox URL","path":"/init","code":404}"#,
            ),
            (
                ErrorPage::missing_traffic_access_token(SANDBOX, HOST, "e2b-traffic-access-token"),
                r#"{"sandboxId":"im9r2ycjiy2534qsdy1oo","message":"Sandbox is secured with traffic access token. Token header 'e2b-traffic-access-token' is missing","code":403}"#,
            ),
            (
                ErrorPage::invalid_traffic_access_token(SANDBOX, HOST, "e2b-traffic-access-token"),
                r#"{"sandboxId":"im9r2ycjiy2534qsdy1oo","message":"Sandbox is secured with traffic access token. Provided token in header 'e2b-traffic-access-token' is invalid","code":403}"#,
            ),
            (
                ErrorPage::team_sandbox_limit(SANDBOX, HOST, ""),
                r#"{"sandboxId":"im9r2ycjiy2534qsdy1oo","message":"Sandbox limit reached","code":429}"#,
            ),
        ];
        for (page, expected) in cases {
            assert_eq!(read_text(page.respond(&headers(&[])).unwrap()).await, expected);
        }
    }
}
