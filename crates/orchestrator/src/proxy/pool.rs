pub mod client;
pub mod connector;
pub mod proxy_env;
pub mod tls;
pub mod tunnel;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use super::destination::Destination;
use client::ProxyClient;

const HOST_CONNECTION_SPLIT: usize = 4;

#[derive(Debug, Clone)]
pub struct PoolSettings {
    pub max_client_conns: usize,
    pub max_connection_attempts: usize,
    pub idle_timeout: Duration,
    pub disable_keep_alives: bool,
    pub proxy_env: Arc<proxy_env::ProxyEnv>,
}

impl PoolSettings {
    fn max_host_idle_conns(&self) -> usize {
        match self.max_client_conns <= HOST_CONNECTION_SPLIT {
            true => self.max_client_conns,
            false => self.max_client_conns / HOST_CONNECTION_SPLIT,
        }
    }
}

pub struct ProxyPool {
    clients: Mutex<HashMap<String, Arc<ProxyClient>>>,
    settings: PoolSettings,
    total_connections: Arc<AtomicU64>,
    current_connections: Arc<AtomicI64>,
}

impl ProxyPool {
    pub fn new(settings: PoolSettings) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            settings,
            total_connections: Arc::default(),
            current_connections: Arc::default(),
        }
    }

    pub fn get(&self, destination: &Destination) -> Arc<ProxyClient> {
        self.clients
            .lock()
            .entry(destination.connection_key.clone())
            .or_insert_with(|| {
                Arc::new(ProxyClient::new(
                    &self.settings,
                    destination.insecure_skip_tls_verify,
                    self.total_connections.clone(),
                    self.current_connections.clone(),
                ))
            })
            .clone()
    }

    pub fn close(&self, connection_key: &str) {
        let removed = self.clients.lock().remove(connection_key);
        removed.iter().for_each(|client| client.reset_all_connections());
    }

    pub fn total_connections(&self) -> u64 {
        self.total_connections.load(Ordering::SeqCst)
    }

    pub fn current_connections(&self) -> i64 {
        self.current_connections.load(Ordering::SeqCst)
    }

    pub fn size(&self) -> usize {
        self.clients.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::uri::{Authority, Scheme};

    use super::*;

    fn pool() -> ProxyPool {
        ProxyPool::new(PoolSettings {
            max_client_conns: 16384,
            max_connection_attempts: 1,
            idle_timeout: Duration::from_secs(1),
            disable_keep_alives: false,
            proxy_env: Arc::default(),
        })
    }

    fn destination(connection_key: &str, insecure_skip_tls_verify: bool) -> Destination {
        Destination::builder()
            .scheme(Scheme::HTTPS)
            .authority(Authority::from_static("127.0.0.1:8443"))
            .sandbox_id("sandbox")
            .connection_key(connection_key)
            .insecure_skip_tls_verify(insecure_skip_tls_verify)
            .build()
    }

    #[tokio::test]
    async fn reuses_the_client_across_insecure_skip_tls_verify() {
        for first in [true, false] {
            let pool = pool();
            let a = pool.get(&destination("shared", first));
            let b = pool.get(&destination("shared", !first));
            assert!(Arc::ptr_eq(&a, &b));
            assert_eq!(pool.size(), 1);
        }
    }

    #[tokio::test]
    async fn builds_one_client_per_connection_key() {
        let pool = pool();
        let a = pool.get(&destination("a", false));
        let b = pool.get(&destination("b", false));
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(pool.size(), 2);
        pool.close("a");
        assert_eq!(pool.size(), 1);
    }

    #[test]
    fn splits_host_idle_connections() {
        let split = |max_client_conns| {
            PoolSettings {
                max_client_conns,
                max_connection_attempts: 1,
                idle_timeout: Duration::from_secs(1),
                disable_keep_alives: false,
                proxy_env: Arc::default(),
            }
            .max_host_idle_conns()
        };
        assert_eq!(split(16384), 4096);
        assert_eq!(split(3), 3);
        assert_eq!(split(4), 4);
    }
}
