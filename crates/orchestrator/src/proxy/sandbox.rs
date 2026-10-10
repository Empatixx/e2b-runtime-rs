pub mod internal_routes;
pub mod metrics;
#[cfg(target_os = "linux")]
pub mod node;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::request::Parts;
use axum::http::uri::{Authority, Scheme};
use percent_encoding::percent_decode_str;
use subtle::ConstantTimeEq;

use super::destination::{Destination, MASK_REQUEST_HOST_PORT_PLACEHOLDER};
use super::handler::Resolver;
use super::host::target_from_request;
use super::limit::ConnectionLimit;
use super::limiter::ConnectionLimiter;
use super::pool::proxy_env::ProxyEnv;
use super::refusal::Refusal;
use super::server::{Acceptor, ProxyServer, ProxySettings, ShutdownError};
use crate::envd::ENVD_PORT;
use crate::proto::SandboxNetworkIngressConfig;
use metrics::ProxyMetrics;

pub const TRAFFIC_ACCESS_TOKEN_HEADER: &str = "e2b-traffic-access-token";
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(620);
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
pub const SANDBOX_PROXY_RETRIES: usize = 5;

#[derive(Debug, Clone)]
pub struct SandboxRoute {
    pub sandbox_id: String,
    pub lifecycle_id: String,
    pub host_ip: Ipv4Addr,
    pub ingress: Option<SandboxNetworkIngressConfig>,
}

pub type SandboxLookup = Arc<dyn Fn(&str) -> Option<SandboxRoute> + Send + Sync>;

pub fn scheme_for_port(ingress: Option<&SandboxNetworkIngressConfig>, port: u64) -> Scheme {
    let listed = ingress.is_some_and(|ingress| ingress.https_ports.iter().any(|https| u64::from(*https) == port));
    match port == u64::from(ENVD_PORT) || !listed {
        true => Scheme::HTTP,
        false => Scheme::HTTPS,
    }
}

pub fn destination_resolver(lookup: SandboxLookup) -> Resolver {
    Arc::new(move |parts| resolve_destination(&lookup, parts))
}

pub fn resolve_destination(lookup: &SandboxLookup, parts: &Parts) -> Result<Destination, Refusal> {
    let host = super::handler::request_host(parts);
    let target = target_from_request(&host, &parts.headers)?;
    let is_non_envd_traffic = target.port != u64::from(ENVD_PORT);
    let path = percent_decode_str(parts.uri.path()).decode_utf8_lossy().into_owned();
    if !is_non_envd_traffic && internal_routes::is_internal_path(&path) {
        return Err(Refusal::InternalRoute {
            sandbox_id: target.sandbox_id,
            path,
        });
    }
    let route = lookup(&target.sandbox_id).ok_or_else(|| Refusal::SandboxNotFound {
        sandbox_id: target.sandbox_id.clone(),
    })?;
    if is_non_envd_traffic {
        check_traffic_access_token(&route, parts)?;
    }
    destination_of(&route, target.port, is_non_envd_traffic)
}

fn check_traffic_access_token(route: &SandboxRoute, parts: &Parts) -> Result<(), Refusal> {
    let Some(expected) = route
        .ingress
        .as_ref()
        .and_then(|ingress| ingress.traffic_access_token.as_deref())
        .filter(|token| !token.is_empty())
    else {
        return Ok(());
    };
    match parts
        .headers
        .get(TRAFFIC_ACCESS_TOKEN_HEADER)
        .filter(|value| !value.is_empty())
    {
        None => Err(Refusal::MissingTrafficAccessToken {
            sandbox_id: route.sandbox_id.clone(),
            header: TRAFFIC_ACCESS_TOKEN_HEADER,
        }),
        Some(provided) => bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
            .then_some(())
            .ok_or_else(|| Refusal::InvalidTrafficAccessToken {
                sandbox_id: route.sandbox_id.clone(),
                header: TRAFFIC_ACCESS_TOKEN_HEADER,
            }),
    }
}

fn destination_of(route: &SandboxRoute, port: u64, is_non_envd_traffic: bool) -> Result<Destination, Refusal> {
    let ingress = route.ingress.as_ref();
    let authority = Authority::try_from(format!("{}:{port}", route.host_ip))
        .map_err(|e| Refusal::Unexpected { message: e.to_string() })?;
    Ok(Destination::builder()
        .scheme(scheme_for_port(ingress, port))
        .authority(authority)
        .sandbox_id(route.sandbox_id.clone())
        .sandbox_port(port)
        .default_to_port_error(true)
        .connection_key(route.lifecycle_id.clone())
        .maybe_mask_request_host(mask_request_host(ingress, port, is_non_envd_traffic))
        .insecure_skip_tls_verify(ingress.is_some_and(|ingress| !ingress.https_ports.is_empty()))
        .build())
}

fn mask_request_host(
    ingress: Option<&SandboxNetworkIngressConfig>,
    port: u64,
    is_non_envd_traffic: bool,
) -> Option<String> {
    ingress
        .and_then(|ingress| ingress.mask_request_host.as_deref())
        .filter(|host| is_non_envd_traffic && !host.is_empty())
        .map(|host| host.replace(MASK_REQUEST_HOST_PORT_PLACEHOLDER, &port.to_string()))
}

pub struct SandboxProxy {
    server: Arc<ProxyServer>,
    limiter: Arc<ConnectionLimiter>,
    _metrics: ProxyMetrics,
}

impl SandboxProxy {
    pub fn new(lookup: SandboxLookup, max_incoming_connections: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        let limiter = Arc::new(ConnectionLimiter::default());
        let recorder = metrics::ConnectionRecorder::register();
        let limit = ConnectionLimit::builder()
            .limiter(limiter.clone())
            .max_limit(max_incoming_connections)
            .on_acquired(recorder.on_acquired())
            .on_released(recorder.on_released())
            .on_blocked(recorder.on_blocked())
            .build();
        let server = ProxyServer::new(
            ProxySettings {
                max_connection_attempts: SANDBOX_PROXY_RETRIES,
                idle_timeout: IDLE_TIMEOUT,
                disable_keep_alives: true,
                proxy_env: Arc::new(ProxyEnv::from_process()),
            },
            destination_resolver(lookup),
            Some(limit),
        );
        Self {
            _metrics: ProxyMetrics::register(&server),
            server,
            limiter,
        }
    }

    pub async fn serve(&self, acceptor: impl Acceptor) -> Result<(), String> {
        self.server.serve(acceptor).await.map_err(|e| e.to_string())
    }

    pub async fn close(&self, forced: bool) -> Result<(), String> {
        let mut errors = Vec::new();
        let mut forced = forced;
        if !forced {
            match self.server.shutdown(SHUTDOWN_TIMEOUT).await {
                Ok(()) => {}
                Err(error) => {
                    forced = true;
                    tracing::warn!(?error, "sandbox proxy graceful shutdown interrupted");
                    if let ShutdownError::Listener { message } = error {
                        errors.push(message);
                    }
                }
            }
        }
        if forced {
            errors.extend(self.server.close().err());
        }
        tracing::info!(forced, error = ?errors, "sandbox proxy shutdown complete");
        match errors.is_empty() {
            true => Ok(()),
            false => Err(format!("failed to shutdown proxy server: {}", errors.join("\n"))),
        }
    }

    pub fn remove_from_pool(&self, connection_key: &str) {
        self.server.remove_from_pool(connection_key);
    }

    pub fn on_network_release(&self, lifecycle_id: &str) {
        self.limiter.remove(lifecycle_id);
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Request;

    use super::*;

    const SANDBOX_ID: &str = "im9r2ycjiy2534qsdy1oo";
    const USER_PORT: u64 = 8000;

    fn parts(port: u64, path: &str, headers: &[(&str, &str)]) -> Parts {
        let mut request = Request::builder()
            .uri(path)
            .header("host", format!("{port}-{SANDBOX_ID}.e2b.app"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(()).unwrap().into_parts().0
    }

    fn ingress(token: Option<&str>, mask: Option<&str>, https_ports: &[u32]) -> SandboxNetworkIngressConfig {
        SandboxNetworkIngressConfig {
            traffic_access_token: token.map(str::to_owned),
            mask_request_host: mask.map(str::to_owned),
            https_ports: https_ports.to_vec(),
        }
    }

    fn lookup_of(ingress: Option<SandboxNetworkIngressConfig>) -> SandboxLookup {
        Arc::new(move |sandbox_id| {
            (sandbox_id == SANDBOX_ID).then(|| SandboxRoute {
                sandbox_id: SANDBOX_ID.to_owned(),
                lifecycle_id: "lifecycle-1".to_owned(),
                host_ip: Ipv4Addr::new(10, 11, 0, 2),
                ingress: ingress.clone(),
            })
        })
    }

    fn empty_lookup() -> SandboxLookup {
        Arc::new(|_| None)
    }

    #[test]
    fn scheme_follows_https_ports_but_never_for_envd() {
        let envd = u64::from(ENVD_PORT);
        assert_eq!(scheme_for_port(None, 8080), Scheme::HTTP);
        assert_eq!(scheme_for_port(Some(&ingress(None, None, &[])), 8080), Scheme::HTTP);
        assert_eq!(
            scheme_for_port(Some(&ingress(None, None, &[443, 8443])), 8443),
            Scheme::HTTPS
        );
        assert_eq!(
            scheme_for_port(Some(&ingress(None, None, &[443, 8443])), 8080),
            Scheme::HTTP
        );
        assert_eq!(
            scheme_for_port(Some(&ingress(None, None, &[u32::from(ENVD_PORT)])), envd),
            Scheme::HTTP
        );
    }

    #[test]
    fn control_routes_are_refused_on_the_envd_port_only() {
        let envd = u64::from(ENVD_PORT);
        let cases = [
            (envd, "/init", true),
            (envd, "/freeze", true),
            (envd, "/upgrade", true),
            (envd, "/files", false),
            (envd, "/health", false),
            (USER_PORT, "/init", false),
            (USER_PORT, "/upgrade", false),
        ];
        for (port, path, refused) in cases {
            let outcome = resolve_destination(&empty_lookup(), &parts(port, path, &[]));
            match refused {
                true => assert_eq!(
                    outcome.err(),
                    Some(Refusal::InternalRoute {
                        sandbox_id: SANDBOX_ID.to_owned(),
                        path: path.to_owned()
                    }),
                    "{port} {path}"
                ),
                false => assert_eq!(
                    outcome.err(),
                    Some(Refusal::SandboxNotFound {
                        sandbox_id: SANDBOX_ID.to_owned()
                    }),
                    "{port} {path}"
                ),
            }
        }
    }

    #[test]
    fn control_routes_are_refused_before_the_sandbox_lookup_and_after_decoding() {
        let outcome = resolve_destination(&empty_lookup(), &parts(u64::from(ENVD_PORT), "/%69nit", &[]));
        assert!(matches!(outcome, Err(Refusal::InternalRoute { .. })));
    }

    #[test]
    fn a_traffic_access_token_guards_non_envd_traffic() {
        let lookup = lookup_of(Some(ingress(Some("secret"), None, &[])));
        let header = TRAFFIC_ACCESS_TOKEN_HEADER;
        assert_eq!(
            resolve_destination(&lookup, &parts(USER_PORT, "/", &[])).err(),
            Some(Refusal::MissingTrafficAccessToken {
                sandbox_id: SANDBOX_ID.to_owned(),
                header
            })
        );
        assert_eq!(
            resolve_destination(&lookup, &parts(USER_PORT, "/", &[(header, "")])).err(),
            Some(Refusal::MissingTrafficAccessToken {
                sandbox_id: SANDBOX_ID.to_owned(),
                header
            })
        );
        for wrong in ["wrong", "secre", "secrets"] {
            assert_eq!(
                resolve_destination(&lookup, &parts(USER_PORT, "/", &[(header, wrong)])).err(),
                Some(Refusal::InvalidTrafficAccessToken {
                    sandbox_id: SANDBOX_ID.to_owned(),
                    header
                }),
                "{wrong}"
            );
        }
        assert!(resolve_destination(&lookup, &parts(USER_PORT, "/", &[(header, "secret")])).is_ok());
    }

    #[test]
    fn the_envd_port_is_exempt_from_the_traffic_access_token() {
        let lookup = lookup_of(Some(ingress(Some("secret"), None, &[])));
        assert!(resolve_destination(&lookup, &parts(u64::from(ENVD_PORT), "/files", &[])).is_ok());
    }

    #[test]
    fn an_empty_or_absent_token_leaves_traffic_open() {
        for ingress in [None, Some(ingress(None, None, &[])), Some(ingress(Some(""), None, &[]))] {
            let lookup = lookup_of(ingress);
            assert!(resolve_destination(&lookup, &parts(USER_PORT, "/", &[])).is_ok());
        }
    }

    #[test]
    fn mask_request_host_replaces_the_port_placeholder_for_non_envd_traffic() {
        let lookup = lookup_of(Some(ingress(None, Some("localhost:${PORT}"), &[])));
        let masked = resolve_destination(&lookup, &parts(USER_PORT, "/", &[])).unwrap();
        assert_eq!(masked.mask_request_host.as_deref(), Some("localhost:8000"));
        let envd = resolve_destination(&lookup, &parts(u64::from(ENVD_PORT), "/files", &[])).unwrap();
        assert_eq!(envd.mask_request_host, None);
    }

    #[test]
    fn an_empty_mask_request_host_masks_nothing() {
        let lookup = lookup_of(Some(ingress(None, Some(""), &[])));
        assert_eq!(
            resolve_destination(&lookup, &parts(USER_PORT, "/", &[]))
                .unwrap()
                .mask_request_host,
            None
        );
    }

    #[test]
    fn the_destination_targets_the_slot_and_pools_by_lifecycle() {
        let lookup = lookup_of(Some(ingress(None, None, &[8443])));
        let plain = resolve_destination(&lookup, &parts(USER_PORT, "/", &[])).unwrap();
        assert_eq!(plain.authority.as_str(), "10.11.0.2:8000");
        assert_eq!(plain.scheme, Scheme::HTTP);
        assert_eq!(plain.connection_key, "lifecycle-1");
        assert_eq!(plain.sandbox_id, SANDBOX_ID);
        assert_eq!(plain.sandbox_port, USER_PORT);
        assert!(plain.default_to_port_error);
        assert!(plain.insecure_skip_tls_verify);
        let secure = resolve_destination(&lookup, &parts(8443, "/", &[])).unwrap();
        assert_eq!(secure.scheme, Scheme::HTTPS);
    }

    #[test]
    fn plain_ingress_keeps_tls_verification_on() {
        let lookup = lookup_of(None);
        assert!(
            !resolve_destination(&lookup, &parts(USER_PORT, "/", &[]))
                .unwrap()
                .insecure_skip_tls_verify
        );
    }

    #[test]
    fn unknown_sandboxes_are_not_found() {
        assert_eq!(
            resolve_destination(&empty_lookup(), &parts(USER_PORT, "/", &[])).err(),
            Some(Refusal::SandboxNotFound {
                sandbox_id: SANDBOX_ID.to_owned()
            })
        );
    }
}
