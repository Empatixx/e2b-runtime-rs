use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64};

use axum::http::{Request, Response};
use hyper::body::Incoming;
use hyper_util::client::legacy::{Client, Error};
use hyper_util::rt::{TokioExecutor, TokioTimer};

use super::connector::Connector;
use super::{PoolSettings, tls};
use crate::proxy::tracking::Connections;

pub struct ProxyClient {
    client: Client<Connector, Incoming>,
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
            connections: connections.clone(),
            total_connections,
        };
        let max_idle_per_host = match settings.disable_keep_alives {
            true => 0,
            false => settings.max_host_idle_conns(),
        };
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(settings.idle_timeout)
            .pool_max_idle_per_host(max_idle_per_host)
            .build(connector);
        Self { client, connections }
    }

    pub async fn send(&self, request: Request<Incoming>) -> Result<Response<Incoming>, Error> {
        self.client.request(request).await
    }

    pub fn reset_all_connections(&self) {
        self.connections.abort_all();
    }
}
