use std::net::IpAddr;

use axum::http::HeaderMap;

use super::refusal::Refusal;

const SANDBOX_SHARED_HOST_SUBDOMAIN: &str = "sandbox.";
const HEADER_SANDBOX_ID: &str = "e2b-sandbox-id";
const HEADER_SANDBOX_PORT: &str = "e2b-sandbox-port";
const MISSING_ID_HEADER: &str = "E2b-Sandbox-Id";
const MISSING_PORT_HEADER: &str = "E2b-Sandbox-Port";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub sandbox_id: String,
    pub port: u64,
}

pub fn target_from_request(host: &str, headers: &HeaderMap) -> Result<Target, Refusal> {
    let target = match routes_by_headers(host, headers) {
        true => target_from_headers(headers),
        false => target_from_host(host),
    }?;
    valid_sandbox_id(&target.sandbox_id)
        .then_some(target)
        .ok_or(Refusal::InvalidSandboxId)
}

pub fn request_hostname(host: &str) -> &str {
    match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split_once(']').map_or(bracketed, |(inside, _)| inside),
        None => host
            .rsplit_once(':')
            .map_or(host, |(name, port)| match port.bytes().all(|b| b.is_ascii_digit()) {
                true => name,
                false => host,
            }),
    }
}

pub fn shared_host_domain(host: &str) -> Option<&str> {
    request_hostname(host)
        .strip_prefix(SANDBOX_SHARED_HOST_SUBDOMAIN)
        .filter(|domain| !domain.is_empty())
}

fn routes_by_headers(host: &str, headers: &HeaderMap) -> bool {
    (is_local_request_host(host) || shared_host_domain(host).is_some()) && has_routing_headers(headers)
}

fn is_local_request_host(host: &str) -> bool {
    let hostname = request_hostname(host);
    hostname == "localhost" || hostname.parse::<IpAddr>().is_ok()
}

fn has_routing_headers(headers: &HeaderMap) -> bool {
    [HEADER_SANDBOX_ID, HEADER_SANDBOX_PORT]
        .iter()
        .any(|name| header_text(headers, name).is_some())
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .filter(|value| !value.is_empty())
}

fn target_from_headers(headers: &HeaderMap) -> Result<Target, Refusal> {
    match (
        header_text(headers, HEADER_SANDBOX_ID),
        header_text(headers, HEADER_SANDBOX_PORT),
    ) {
        (None, _) => Err(Refusal::MissingHeader {
            header: MISSING_ID_HEADER,
        }),
        (_, None) => Err(Refusal::MissingHeader {
            header: MISSING_PORT_HEADER,
        }),
        (Some(sandbox_id), Some(port)) => parse_port(&port).map(|port| Target { sandbox_id, port }),
    }
}

fn target_from_host(host: &str) -> Result<Target, Refusal> {
    let (label, _) = host.split_once('.').ok_or(Refusal::InvalidHost)?;
    let mut parts = label.split('-');
    match (parts.next(), parts.next()) {
        (Some(port), Some(sandbox_id)) => parse_port(port).map(|port| Target {
            sandbox_id: sandbox_id.to_owned(),
            port,
        }),
        _ => Err(Refusal::InvalidHost),
    }
}

fn parse_port(raw: &str) -> Result<u64, Refusal> {
    Some(raw)
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .ok_or_else(|| Refusal::InvalidSandboxPort { port: raw.to_owned() })
}

fn valid_sandbox_id(sandbox_id: &str) -> bool {
    !sandbox_id.is_empty() && sandbox_id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderName, HeaderValue};

    use super::*;

    const SANDBOX: &str = "isv6ril5xadwn1k9t2jye";

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

    fn target(host: &str, pairs: &[(&str, &str)]) -> Result<(String, u64), Refusal> {
        target_from_request(host, &headers(pairs)).map(|target| (target.sandbox_id, target.port))
    }

    fn routed(host: &str, port: u64) {
        assert_eq!(target(host, &[]), Ok((SANDBOX.to_owned(), port)), "{host}");
    }

    #[test]
    fn parses_sandbox_hosts() {
        routed("49983-isv6ril5xadwn1k9t2jye-6532622b.e2b.app", 49983);
        routed("49983-isv6ril5xadwn1k9t2jye-6532622b.e2b-test.app", 49983);
        routed("49983-isv6ril5xadwn1k9t2jye-6532622b.demo.e2b.app", 49983);
        routed("49983-isv6ril5xadwn1k9t2jye.e2b.app", 49983);
        routed("49983-isv6ril5xadwn1k9t2jye.e2b-test.app", 49983);
        routed("49983-isv6ril5xadwn1k9t2jye.demo.e2b.app", 49983);
    }

    #[test]
    fn rejects_hosts_without_port_part() {
        assert_eq!(target("isv6ril5xadwn1k9t2jye.e2b.app", &[]), Err(Refusal::InvalidHost));
    }

    #[test]
    fn rejects_invalid_port_part() {
        assert_eq!(
            target("abcd-isv6ril5xadwn1k9t2jye.e2b.app", &[]),
            Err(Refusal::InvalidSandboxPort {
                port: "abcd".to_owned()
            })
        );
        assert!(matches!(
            target("+80-isv6ril5xadwn1k9t2jye.e2b.app", &[]),
            Err(Refusal::InvalidSandboxPort { .. })
        ));
    }

    #[test]
    fn rejects_hosts_without_domain() {
        for host in ["49983-isv6ril5xadwn1k9t2jye", "49983-isv6ril5xadwn1k9t2jye:8080"] {
            assert_eq!(target(host, &[]), Err(Refusal::InvalidHost), "{host}");
        }
    }

    #[test]
    fn routes_by_headers_on_local_and_ip_hosts() {
        let routing = [("e2b-sandbox-id", SANDBOX), ("e2b-sandbox-port", "8080")];
        for host in ["localhost:1234", "127.0.0.1:1234", "[::1]:1234"] {
            assert_eq!(target(host, &routing), Ok((SANDBOX.to_owned(), 8080)), "{host}");
        }
        let proxied = [("e2b-sandbox-id", SANDBOX), ("e2b-sandbox-port", "49983")];
        for host in [
            "sandbox.e2b.app",
            "192.168.100.66:3002",
            "10.0.0.5:3002",
            "34.120.5.10:3002",
        ] {
            assert_eq!(target(host, &proxied), Ok((SANDBOX.to_owned(), 49983)), "{host}");
        }
    }

    #[test]
    fn rejects_invalid_sandbox_ids_in_headers() {
        for id in ["sbx:123", "SBX_UPPER"] {
            let routing = [("e2b-sandbox-id", id), ("e2b-sandbox-port", "8080")];
            assert_eq!(
                target("localhost:1234", &routing),
                Err(Refusal::InvalidSandboxId),
                "{id}"
            );
        }
    }

    #[test]
    fn reports_the_missing_routing_header() {
        assert_eq!(
            target("localhost:1234", &[("e2b-sandbox-port", "8080")]),
            Err(Refusal::MissingHeader {
                header: "E2b-Sandbox-Id"
            })
        );
        assert_eq!(
            target("localhost:1234", &[("e2b-sandbox-id", SANDBOX)]),
            Err(Refusal::MissingHeader {
                header: "E2b-Sandbox-Port"
            })
        );
    }

    #[test]
    fn ignores_routing_headers_on_regular_sandbox_hosts() {
        let routing = [
            ("e2b-sandbox-id", "iother5b5aiixd410phsjv"),
            ("e2b-sandbox-port", "3000"),
        ];
        assert_eq!(
            target("49983-isv6ril5xadwn1k9t2jye.e2b.app", &routing),
            Ok((SANDBOX.to_owned(), 49983))
        );
    }

    #[test]
    fn hostname_drops_port_and_brackets() {
        assert_eq!(request_hostname("localhost:1234"), "localhost");
        assert_eq!(request_hostname("[::1]:1234"), "::1");
        assert_eq!(request_hostname("sandbox.e2b.app"), "sandbox.e2b.app");
    }

    #[test]
    fn shared_host_needs_a_domain() {
        assert_eq!(shared_host_domain("sandbox.e2b.app:80"), Some("e2b.app"));
        assert_eq!(shared_host_domain("sandbox."), None);
        assert_eq!(shared_host_domain("e2b.app"), None);
    }
}
