use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64};

use axum::http::header::{HeaderValue, PROXY_AUTHORIZATION};
use axum::http::{Request, Response, Uri};
use hyper::body::Incoming;
use hyper_util::client::legacy::{Client, Error};
use hyper_util::rt::{TokioExecutor, TokioTimer};

use super::connector::Connector;
use super::proxy_env::{ProxyEnv, ProxyScheme};
use super::{PoolSettings, tls};
use crate::proxy::tracking::Connections;

pub struct ProxyClient {
    client: Client<Connector, Incoming>,
    proxy_env: Arc<ProxyEnv>,
    connections: Connections,
}

impl ProxyClient {
    pub fn new(
        settings: &PoolSettings,
        insecure_skip_tls_verify: bool,
        total_connections: Arc<AtomicU64>,
        current_connections: Arc<AtomicI64>,
    ) -> Self {
        let connections = Connections::sharing_counter(current_connections);
        let connector = Connector {
            max_attempts: settings.max_connection_attempts,
            tls: tls::connector(insecure_skip_tls_verify),
            proxy_tls: tls::connector(false),
            proxy_env: settings.proxy_env.clone(),
            connections: connections.clone(),
            total_connections,
        };
        let max_idle_per_host = match settings.disable_keep_alives {
            true => 0,
            false => settings.max_host_idle_conns(),
        };
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .http1_title_case_headers(true)
            .pool_idle_timeout(settings.idle_timeout)
            .pool_max_idle_per_host(max_idle_per_host)
            .build(connector);
        Self {
            client,
            proxy_env: settings.proxy_env.clone(),
            connections,
        }
    }

    pub async fn send(&self, mut request: Request<Incoming>) -> Result<Response<Incoming>, Error> {
        if let Some(authorization) = self.proxy_authorization(request.uri()) {
            request.headers_mut().insert(PROXY_AUTHORIZATION, authorization);
        }
        self.client.request(request).await
    }

    fn proxy_authorization(&self, uri: &Uri) -> Option<HeaderValue> {
        let host = uri.host()?.trim_matches(['[', ']']);
        let proxy = match uri.scheme_str() {
            Some("http") => self
                .proxy_env
                .for_target(false, host, uri.port_u16().unwrap_or(80))
                .ok()
                .flatten(),
            _ => None,
        }?;
        proxy
            .basic_authorization()
            .filter(|_| proxy.scheme != ProxyScheme::Socks5)
            .and_then(|credentials| HeaderValue::from_str(&credentials).ok())
    }

    pub fn reset_all_connections(&self) {
        self.connections.abort_all();
    }
}
