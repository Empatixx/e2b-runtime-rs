use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use parking_lot::Mutex;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use super::activity::Activity;
use super::body::HeldBody;
use super::handler::{Handler, Resolver};
use super::limit::ConnectionLimit;
use super::pool::proxy_env::ProxyEnv;
use super::pool::{PoolSettings, ProxyPool};
use super::tracking::{Connections, Probe, Protocol};

const MAX_HEADER_BYTES: usize = (1 << 20) + 4096;
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

pub trait Acceptor: Send + Sync + 'static {
    fn accept(&self) -> BoxFuture<'_, io::Result<TcpStream>>;
    fn close(&self) -> io::Result<()>;
}

impl Acceptor for TcpListener {
    fn accept(&self) -> BoxFuture<'_, io::Result<TcpStream>> {
        Box::pin(async move { TcpListener::accept(self).await.map(|(stream, _)| stream) })
    }

    fn close(&self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownError {
    DeadlineExceeded,
    Listener { message: String },
}

#[derive(Debug, Clone)]
pub struct ProxySettings {
    pub max_connection_attempts: usize,
    pub idle_timeout: Duration,
    pub disable_keep_alives: bool,
    pub proxy_env: Arc<ProxyEnv>,
}

pub struct ProxyServer {
    handler: Handler,
    pool: Arc<ProxyPool>,
    connections: Connections,
    closing: watch::Sender<Closing>,
    listeners: Mutex<Vec<Arc<dyn Acceptor>>>,
    idle_timeout: Duration,
}

impl ProxyServer {
    pub fn new(settings: ProxySettings, resolve: Resolver, limit: Option<ConnectionLimit>) -> Arc<Self> {
        let pool = Arc::new(ProxyPool::new(PoolSettings {
            max_client_conns: MAX_CLIENT_CONNS,
            max_connection_attempts: settings.max_connection_attempts,
            idle_timeout: settings.idle_timeout,
            disable_keep_alives: settings.disable_keep_alives,
            proxy_env: settings.proxy_env.clone(),
        }));
        Arc::new(Self {
            handler: Handler::new(pool.clone(), resolve, limit, settings.disable_keep_alives),
            pool,
            connections: Connections::default(),
            closing: watch::channel(Closing::Open).0,
            listeners: Mutex::new(Vec::new()),
            idle_timeout: settings.idle_timeout + IDLE_TIMEOUT_BUFFER_UPSTREAM_DOWNSTREAM,
        })
    }

    pub async fn serve(self: &Arc<Self>, acceptor: impl Acceptor) -> io::Result<()> {
        let acceptor: Arc<dyn Acceptor> = Arc::new(acceptor);
        self.listeners.lock().push(acceptor.clone());
        let mut closing = self.closing.subscribe();
        loop {
            tokio::select! {
                accepted = acceptor.accept() => match accepted {
                    Ok(stream) => drop(tokio::spawn(self.clone().serve_connection(stream))),
                    Err(error) => {
                        tracing::warn!(%error, "sandbox proxy accept failed");
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                },
                _ = closing.changed() => return Ok(()),
            }
        }
    }

    pub async fn shutdown(&self, grace: Duration) -> Result<(), ShutdownError> {
        self.closing.send_replace(Closing::Graceful);
        let listener_error = self.close_listeners();
        tokio::time::timeout(grace, self.wait_for_connections())
            .await
            .map_err(|_| ShutdownError::DeadlineExceeded)?;
        listener_error.map_or(Ok(()), |message| Err(ShutdownError::Listener { message }))
    }

    pub fn close(&self) -> Result<(), String> {
        self.closing.send_replace(Closing::Forced);
        let listener_error = self.close_listeners();
        self.connections.abort_all();
        listener_error.map_or(Ok(()), Err)
    }

    fn close_listeners(&self) -> Option<String> {
        let listeners = std::mem::take(&mut *self.listeners.lock());
        listeners
            .iter()
            .filter_map(|listener| listener.close().err())
            .map(|error| error.to_string())
            .next()
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
        let probe = Probe::new();
        let mut probed = probe.subscribe();
        let io = TokioIo::new(self.connections.track_probed(stream, probe));
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
        builder
            .http1()
            .timer(TokioTimer::new())
            .max_buf_size(MAX_HEADER_BYTES)
            .header_read_timeout(None);
        let connection = builder.serve_connection_with_upgrades(io, service);
        tokio::pin!(connection);
        let mut closing = self.closing.subscribe();
        let mut shutdown_requested = *closing.borrow() == Closing::Graceful;
        let mut graceful_sent = false;
        loop {
            if shutdown_requested && !graceful_sent && !probed.borrow().preface_pending {
                graceful_sent = true;
                connection.as_mut().graceful_shutdown();
            }
            tokio::select! {
                result = connection.as_mut() => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "sandbox proxy connection ended");
                    }
                    return;
                }
                () = activity.idle_for(self.idle_timeout), if !shutdown_requested => {
                    shutdown_requested = true;
                }
                _ = closing.changed(), if !shutdown_requested => {
                    shutdown_requested = true;
                }
                changed = probed.changed() => {
                    if changed.is_ok() && probed.borrow_and_update().protocol == Protocol::H2c {
                        activity.start_idle();
                    }
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
