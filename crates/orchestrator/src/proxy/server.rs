use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use super::activity::Activity;
use super::body::HeldBody;
use super::handler::{Handler, Resolver};
use super::limit::ConnectionLimit;
use super::pool::{PoolSettings, ProxyPool};
use super::tracking::Connections;

const MAX_CLIENT_CONNS: usize = 16384;
const IDLE_TIMEOUT_BUFFER_UPSTREAM_DOWNSTREAM: Duration = Duration::from_secs(10);
const ACCEPT_KEEP_ALIVE: Duration = Duration::from_secs(15);
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);
const SHUTDOWN_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closing {
    Open,
    Graceful,
    Forced,
}

#[derive(Debug, Clone, Copy)]
pub struct ProxySettings {
    pub max_connection_attempts: usize,
    pub idle_timeout: Duration,
    pub disable_keep_alives: bool,
}

pub struct ProxyServer {
    handler: Handler,
    pool: Arc<ProxyPool>,
    connections: Connections,
    closing: watch::Sender<Closing>,
    idle_timeout: Duration,
}

impl ProxyServer {
    pub fn new(settings: ProxySettings, resolve: Resolver, limit: Option<ConnectionLimit>) -> Arc<Self> {
        let pool = Arc::new(ProxyPool::new(PoolSettings {
            max_client_conns: MAX_CLIENT_CONNS,
            max_connection_attempts: settings.max_connection_attempts,
            idle_timeout: settings.idle_timeout,
            disable_keep_alives: settings.disable_keep_alives,
        }));
        Arc::new(Self {
            handler: Handler::new(pool.clone(), resolve, limit, settings.disable_keep_alives),
            pool,
            connections: Connections::default(),
            closing: watch::channel(Closing::Open).0,
            idle_timeout: settings.idle_timeout + IDLE_TIMEOUT_BUFFER_UPSTREAM_DOWNSTREAM,
        })
    }

    pub async fn serve(self: &Arc<Self>, listener: TcpListener) -> io::Result<()> {
        let mut closing = self.closing.subscribe();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => drop(tokio::spawn(self.clone().serve_connection(stream))),
                    Err(error) => {
                        tracing::warn!(%error, "sandbox proxy accept failed");
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                },
                _ = closing.changed() => return Ok(()),
            }
        }
    }

    pub async fn shutdown(&self, grace: Duration) -> Result<(), String> {
        self.closing.send_replace(Closing::Graceful);
        tokio::time::timeout(grace, self.wait_for_connections())
            .await
            .map_err(|_| "context deadline exceeded".to_owned())
    }

    pub fn close(&self) {
        self.closing.send_replace(Closing::Forced);
        self.connections.abort_all();
    }

    pub fn remove_from_pool(&self, connection_key: &str) {
        self.pool.close(connection_key);
    }

    pub fn current_server_connections(&self) -> i64 {
        self.connections.current()
    }

    pub fn total_pool_connections(&self) -> u64 {
        self.pool.total_connections()
    }

    pub fn current_pool_connections(&self) -> i64 {
        self.pool.current_connections()
    }

    pub fn pool_size(&self) -> usize {
        self.pool.size()
    }

    async fn wait_for_connections(&self) {
        while self.connections.current() != 0 {
            tokio::time::sleep(SHUTDOWN_POLL).await;
        }
    }

    async fn serve_connection(self: Arc<Self>, stream: TcpStream) {
        configure(&stream);
        let io = TokioIo::new(self.connections.track(stream, false));
        let handler = self.handler.clone();
        let activity = Activity::new();
        let engaging = activity.clone();
        let service = service_fn(move |request| {
            let handler = handler.clone();
            let engaged = engaging.engage();
            async move {
                let response = handler.handle(request).await;
                Ok::<_, Infallible>(response.map(|body| HeldBody::wrap(body, engaged)))
            }
        });
        let mut builder = auto::Builder::new(TokioExecutor::new());
        builder.http1().timer(TokioTimer::new()).header_read_timeout(None);
        let connection = builder.serve_connection_with_upgrades(io, service);
        tokio::pin!(connection);
        let mut closing = self.closing.subscribe();
        let mut draining = *closing.borrow() == Closing::Graceful;
        loop {
            tokio::select! {
                result = connection.as_mut() => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "sandbox proxy connection ended");
                    }
                    return;
                }
                () = activity.idle_for(self.idle_timeout), if !draining => {
                    draining = true;
                    connection.as_mut().graceful_shutdown();
                }
                changed = closing.changed(), if !draining => {
                    draining = changed.is_ok();
                    connection.as_mut().graceful_shutdown();
                }
            }
        }
    }
}

fn configure(stream: &TcpStream) {
    drop(stream.set_nodelay(true));
    drop(
        SockRef::from(stream).set_tcp_keepalive(
            &TcpKeepalive::new()
                .with_time(ACCEPT_KEEP_ALIVE)
                .with_interval(ACCEPT_KEEP_ALIVE),
        ),
    );
}
