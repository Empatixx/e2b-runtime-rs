use std::sync::Arc;

use axum::http::header::{CONNECTION, HOST, HeaderMap, HeaderName, HeaderValue, UPGRADE, USER_AGENT};
use axum::http::uri::PathAndQuery;
use axum::http::{Request, Response, StatusCode, Uri, Version};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;

use super::body::HeldBody;
use super::body::{BoxError, ProxyBody, empty_body};
use super::content_type::{needs_sniffing, sniff};
use super::destination::Destination;
use super::limit::Permit;
use super::pool::client::ProxyClient;
use super::stream_abort::{StreamEndingBody, ends_streams_on_abort};

const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
const FORWARDED: [&str; 4] = ["forwarded", "x-forwarded-for", "x-forwarded-host", "x-forwarded-proto"];

pub struct Forwarding {
    pub destination: Destination,
    pub host: String,
    pub disable_keep_alives: bool,
    pub permit: Option<Arc<Permit>>,
}

pub async fn forward(
    client: &ProxyClient,
    mut request: Request<Incoming>,
    forwarding: Forwarding,
) -> Result<Response<ProxyBody>, String> {
    let requested = requested_upgrade(request.headers());
    let inbound = requested.as_ref().map(|_| hyper::upgrade::on(&mut request));
    let outbound = outbound_request(request, &forwarding, requested.as_deref())?;
    let response = client.send(outbound).await.map_err(|e| e.to_string())?;
    match response.status() {
        StatusCode::SWITCHING_PROTOCOLS => switch_protocols(response, requested, inbound, forwarding.permit),
        _ => Ok(relay(response, &forwarding).await),
    }
}

fn outbound_request(
    request: Request<Incoming>,
    forwarding: &Forwarding,
    upgrade: Option<&str>,
) -> Result<Request<Incoming>, String> {
    let (mut parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"));
    parts.uri = Uri::builder()
        .scheme(forwarding.destination.scheme.clone())
        .authority(forwarding.destination.authority.clone())
        .path_and_query(path)
        .build()
        .map_err(|e| e.to_string())?;
    parts.version = Version::HTTP_11;
    strip_hop_by_hop(&mut parts.headers);
    FORWARDED.iter().for_each(|name| {
        parts.headers.remove(*name);
    });
    set_host(&mut parts.headers, forwarding)?;
    if !parts.headers.contains_key(USER_AGENT) {
        parts.headers.insert(USER_AGENT, HeaderValue::from_static(""));
    }
    match upgrade {
        Some(kind) => keep_upgrade(&mut parts.headers, kind)?,
        None if forwarding.disable_keep_alives => {
            parts.headers.insert(CONNECTION, HeaderValue::from_static("close"));
        }
        None => {}
    }
    Ok(Request::from_parts(parts, body))
}

fn set_host(headers: &mut HeaderMap, forwarding: &Forwarding) -> Result<(), String> {
    let inbound = header_value(&forwarding.host)?;
    match &forwarding.destination.mask_request_host {
        Some(mask) => {
            headers.insert(HeaderName::from_static("x-forwarded-host"), inbound);
            headers.insert(HOST, header_value(mask)?);
        }
        None => {
            headers.insert(HOST, inbound);
        }
    }
    Ok(())
}

fn header_value(text: &str) -> Result<HeaderValue, String> {
    HeaderValue::from_str(text).map_err(|e| e.to_string())
}

fn keep_upgrade(headers: &mut HeaderMap, kind: &str) -> Result<(), String> {
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, header_value(kind)?);
    Ok(())
}

fn requested_upgrade(headers: &HeaderMap) -> Option<String> {
    connection_tokens(headers)
        .iter()
        .any(|token| token.eq_ignore_ascii_case("upgrade"))
        .then(|| headers.get(UPGRADE))
        .flatten()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CONNECTION)
        .iter()
        .flat_map(|value| {
            String::from_utf8_lossy(value.as_bytes())
                .split(',')
                .map(|token| token.trim().to_owned())
                .collect::<Vec<_>>()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    connection_tokens(headers).iter().for_each(|token| {
        headers.remove(token.as_str());
    });
    HOP_BY_HOP.iter().for_each(|name| {
        headers.remove(*name);
    });
}

async fn relay(response: Response<Incoming>, forwarding: &Forwarding) -> Response<ProxyBody> {
    let (mut parts, body) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    let mut body = body.map_err(BoxError::from).boxed_unsync();
    if needs_sniffing(parts.status, &parts.headers) {
        body = sniff(&mut parts, body).await;
    }
    let body = match ends_streams_on_abort(parts.status, &parts.headers) {
        true => StreamEndingBody::wrap(
            body,
            format!(
                "the connection to sandbox {} ended before the stream completed",
                forwarding.destination.sandbox_id
            ),
        ),
        false => body,
    };
    Response::from_parts(parts, HeldBody::wrap(body, forwarding.permit.clone()))
}

fn switch_protocols(
    mut response: Response<Incoming>,
    requested: Option<String>,
    inbound: Option<OnUpgrade>,
    permit: Option<Arc<Permit>>,
) -> Result<Response<ProxyBody>, String> {
    let answered = response
        .headers()
        .get(UPGRADE)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .unwrap_or_default();
    let requested = requested.unwrap_or_default();
    if !answered.eq_ignore_ascii_case(&requested) {
        return Err(format!(
            "backend tried to switch protocol {answered:?} when {requested:?} was requested"
        ));
    }
    let upstream = hyper::upgrade::on(&mut response);
    let (mut parts, _) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    keep_upgrade(&mut parts.headers, &answered)?;
    tokio::spawn(tunnel(inbound, upstream, permit));
    Ok(Response::from_parts(parts, empty_body()))
}

async fn tunnel(inbound: Option<OnUpgrade>, upstream: OnUpgrade, permit: Option<Arc<Permit>>) {
    let _permit = permit;
    let Some(inbound) = inbound else { return };
    if let (Ok(inbound), Ok(upstream)) = tokio::join!(inbound, upstream) {
        drop(tokio::io::copy_bidirectional(&mut TokioIo::new(inbound), &mut TokioIo::new(upstream)).await);
    }
}
