use std::sync::Arc;

use axum::http::header::{CONNECTION, HOST, HeaderMap, HeaderValue};
use axum::http::request::Parts;
use axum::http::{Method, Request, Response, StatusCode, Version};
use hyper::body::Incoming;

use super::body::ProxyBody;
use super::cors;
use super::destination::Destination;
use super::forward::{Forwarding, forward};
use super::limit::{ConnectionLimit, OverLimit, Permit};
use super::pages::ErrorPage;
use super::pool::ProxyPool;
use super::refusal::Refusal;

pub type Resolver = Arc<dyn Fn(&Parts) -> Result<Destination, Refusal> + Send + Sync>;

#[derive(Clone)]
pub struct Handler {
    pool: Arc<ProxyPool>,
    resolve: Resolver,
    limit: Option<ConnectionLimit>,
    disable_keep_alives: bool,
}

impl Handler {
    pub fn new(
        pool: Arc<ProxyPool>,
        resolve: Resolver,
        limit: Option<ConnectionLimit>,
        disable_keep_alives: bool,
    ) -> Self {
        Self {
            pool,
            resolve,
            limit,
            disable_keep_alives,
        }
    }

    pub async fn handle(&self, request: Request<Incoming>) -> Response<ProxyBody> {
        let (parts, body) = request.into_parts();
        if lacks_required_host(&parts) {
            return missing_host_response();
        }
        let inbound = Inbound::of(&parts);
        let destination = match (self.resolve)(&parts) {
            Ok(destination) => destination,
            Err(refusal) => return refuse(&inbound, refusal),
        };
        let permit = match self.acquire(&destination) {
            Ok(permit) => permit.map(Arc::new),
            Err(over) => return too_many_connections(&inbound, &destination, over),
        };
        tracing::debug!(sandbox_id = %destination.sandbox_id, port = destination.sandbox_port, "proxying request");
        let client = self.pool.get(&destination);
        let forwarding = Forwarding {
            destination: destination.clone(),
            host: inbound.host.clone(),
            disable_keep_alives: self.disable_keep_alives,
            permit,
        };
        match forward(&client, Request::from_parts(parts, body), forwarding).await {
            Ok(response) => response,
            Err(error) => upstream_failure(&inbound, &destination, &error),
        }
    }

    fn acquire(&self, destination: &Destination) -> Result<Option<Permit>, OverLimit> {
        self.limit
            .as_ref()
            .map(|limit| limit.acquire(&destination.connection_key))
            .transpose()
    }
}

fn lacks_required_host(parts: &Parts) -> bool {
    parts.version == Version::HTTP_11 && parts.method != Method::CONNECT && !parts.headers.contains_key(HOST)
}

fn missing_host_response() -> Response<ProxyBody> {
    let mut response = cors::text_response(
        StatusCode::BAD_REQUEST,
        "400 Bad Request: missing required Host header".to_owned(),
    );
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

struct Inbound {
    method: Method,
    headers: HeaderMap,
    host: String,
}

impl Inbound {
    fn of(parts: &Parts) -> Self {
        Self {
            method: parts.method.clone(),
            headers: parts.headers.clone(),
            host: request_host(parts),
        }
    }
}

pub fn request_host(parts: &Parts) -> String {
    parts
        .headers
        .get(HOST)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .or_else(|| parts.uri.authority().map(|authority| authority.as_str().to_owned()))
        .unwrap_or_default()
}

fn refuse(inbound: &Inbound, refusal: Refusal) -> Response<ProxyBody> {
    if let Some(preflight) = cors::handle_preflight(&inbound.method, &inbound.headers) {
        return preflight;
    }
    let host = inbound.host.as_str();
    tracing::warn!(%host, ?refusal, "refusing request");
    match refusal {
        Refusal::MissingHeader { .. } => cors::error("missing header", StatusCode::BAD_REQUEST),
        Refusal::InvalidHost => cors::error("Invalid host", StatusCode::BAD_REQUEST),
        Refusal::InvalidSandboxId => cors::error("Invalid sandbox ID", StatusCode::BAD_REQUEST),
        Refusal::InvalidSandboxPort { .. } => cors::error("Invalid sandbox port", StatusCode::BAD_REQUEST),
        Refusal::SandboxNotFound { sandbox_id } => answer(
            ErrorPage::sandbox_not_found(&sandbox_id, host),
            &inbound.headers,
            "Failed to handle sandbox not found error",
        ),
        Refusal::ResumePermissionDenied { sandbox_id } => answer(
            ErrorPage::resume_permission_denied(&sandbox_id, host),
            &inbound.headers,
            "Failed to handle sandbox resume permission denied error",
        ),
        Refusal::StillTransitioning { sandbox_id } => answer(
            ErrorPage::still_transitioning(&sandbox_id, host),
            &inbound.headers,
            "Failed to handle sandbox still transitioning error",
        ),
        Refusal::InternalRoute { path, .. } => answer(
            ErrorPage::internal_route(host, &path),
            &inbound.headers,
            "Failed to handle internal route error",
        ),
        Refusal::ResourceExhausted { sandbox_id, message } => answer(
            ErrorPage::team_sandbox_limit(&sandbox_id, host, &message),
            &inbound.headers,
            "Failed to handle team sandbox limit error",
        ),
        Refusal::MissingTrafficAccessToken { sandbox_id, header } => answer(
            ErrorPage::missing_traffic_access_token(&sandbox_id, host, header),
            &inbound.headers,
            "Failed to handle invalid missing access token header error",
        ),
        Refusal::InvalidTrafficAccessToken { sandbox_id, header } => answer(
            ErrorPage::invalid_traffic_access_token(&sandbox_id, host, header),
            &inbound.headers,
            "Failed to handle invalid traffic access token header error",
        ),
        Refusal::Unexpected { message } => cors::error(
            &format!("Unexpected error when routing request: {message}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

fn answer(page: ErrorPage, request_headers: &HeaderMap, failure: &str) -> Response<ProxyBody> {
    page.respond(request_headers).unwrap_or_else(|error| {
        tracing::error!(%error, failure, "failed to render error page");
        cors::error(failure, StatusCode::INTERNAL_SERVER_ERROR)
    })
}

fn too_many_connections(inbound: &Inbound, destination: &Destination, over: OverLimit) -> Response<ProxyBody> {
    let host = inbound.host.as_str();
    tracing::warn!(%host, sandbox_id = %destination.sandbox_id, connection_limit = over.max_limit, "sandbox too many incoming connections");
    cors::handle_preflight(&inbound.method, &inbound.headers).unwrap_or_else(|| {
        answer(
            ErrorPage::too_many_connections(&destination.sandbox_id, host, over.max_limit),
            &inbound.headers,
            "Failed to handle too many connections error",
        )
    })
}

fn upstream_failure(inbound: &Inbound, destination: &Destination, error: &str) -> Response<ProxyBody> {
    if let Some(preflight) = cors::handle_preflight(&inbound.method, &inbound.headers) {
        return preflight;
    }
    let host = inbound.host.as_str();
    tracing::warn!(sandbox_id = %destination.sandbox_id, port = destination.sandbox_port, %error, "sandbox error handler called");
    match destination.default_to_port_error {
        true => answer(
            ErrorPage::port_closed(&destination.sandbox_id, host, destination.sandbox_port),
            &inbound.headers,
            "Failed to handle closed port error",
        ),
        false => cors::error("Failed to route request to sandbox", StatusCode::BAD_GATEWAY),
    }
}
