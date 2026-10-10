use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::Uri;
use hyper_util::rt::TokioIo;
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tower_service::Service;

use super::proxy_env::{ProxyEnv, ProxyScheme, ProxyTarget};
use super::tunnel::{Stream, http_connect, socks5_connect};
use crate::proxy::tracking::{Abortable, Connections, Tracked};

const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const KEEP_ALIVE: Duration = Duration::from_secs(20);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_BACKOFF_STEP: Duration = Duration::from_millis(100);

pub struct Tunneled {
    stream: Stream,
    socket: std::net::TcpStream,
    proxied: bool,
}

pub enum Upstream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
    Tunneled(Box<Tunneled>),
}

impl Abortable for Upstream {
    fn reset_on_close(&self) {
        match self {
            Upstream::Plain(stream) => stream.reset_on_close(),
            Upstream::Tls(stream) => stream.get_ref().0.reset_on_close(),
            Upstream::Tunneled(tunnel) => drop(SockRef::from(&tunnel.socket).set_linger(Some(Duration::ZERO))),
        }
    }

    fn proxied(&self) -> bool {
        matches!(self, Upstream::Tunneled(tunnel) if tunnel.proxied)
    }
}

impl AsyncRead for Upstream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Upstream::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
            Upstream::Tunneled(tunnel) => tunnel.stream.as_mut().poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Upstream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Upstream::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Upstream::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
            Upstream::Tunneled(tunnel) => tunnel.stream.as_mut().poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Upstream::Tls(stream) => Pin::new(stream).poll_flush(cx),
            Upstream::Tunneled(tunnel) => tunnel.stream.as_mut().poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Upstream::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
            Upstream::Tunneled(tunnel) => tunnel.stream.as_mut().poll_shutdown(cx),
        }
    }
}

#[derive(Clone)]
pub struct Connector {
    pub max_attempts: usize,
    pub tls: TlsConnector,
    pub proxy_tls: TlsConnector,
    pub proxy_env: Arc<ProxyEnv>,
    pub connections: Connections,
    pub total_connections: Arc<AtomicU64>,
}

impl Service<Uri> for Connector {
    type Response = TokioIo<Tracked<Upstream>>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connector = self.clone();
        Box::pin(async move { connector.connect(&uri).await })
    }
}

impl Connector {
    async fn connect(&self, uri: &Uri) -> io::Result<TokioIo<Tracked<Upstream>>> {
        let https = uri.scheme_str() == Some("https");
        let host = uri.host().unwrap_or_default().trim_matches(['[', ']']).to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let upstream = match self
            .proxy_env
            .for_target(https, &host, port)
            .map_err(io::Error::other)?
        {
            Some(proxy) => self.connect_via(proxy, &host, port, https).await?,
            None => self.connect_direct(uri, https).await?,
        };
        self.total_connections.fetch_add(1, Ordering::SeqCst);
        Ok(TokioIo::new(self.connections.track(upstream, true)))
    }

    async fn connect_direct(&self, uri: &Uri, https: bool) -> io::Result<Upstream> {
        let address = uri
            .authority()
            .map(|authority| authority.as_str().to_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing upstream authority"))?;
        let stream = self.dial_with_retries(&address).await?;
        match https {
            true => self.handshake(stream, uri).await,
            false => Ok(Upstream::Plain(stream)),
        }
    }

    async fn connect_via(&self, proxy: &ProxyTarget, host: &str, port: u16, https: bool) -> io::Result<Upstream> {
        let tcp = self.dial_with_retries(&proxy.address()).await?;
        let std_stream = tcp.into_std()?;
        let socket = std_stream.try_clone()?;
        let mut stream: Stream = Box::pin(TcpStream::from_std(std_stream)?);
        if proxy.scheme == ProxyScheme::Https {
            stream = Box::pin(handshake_with(&self.proxy_tls, stream, &proxy.host).await?);
        }
        let proxied = match (proxy.scheme, https) {
            (ProxyScheme::Socks5, _) => {
                socks5_connect(&mut stream, proxy, host, port).await?;
                false
            }
            (_, true) => {
                http_connect(&mut stream, proxy, host, port).await?;
                false
            }
            (_, false) => true,
        };
        if https {
            stream = Box::pin(handshake_with(&self.tls, stream, host).await?);
        }
        Ok(Upstream::Tunneled(Box::new(Tunneled {
            stream,
            socket,
            proxied,
        })))
    }

    async fn dial_with_retries(&self, address: &str) -> io::Result<TcpStream> {
        let attempts = self.max_attempts.max(1);
        let mut last_error = io::Error::other("no connection attempt was made");
        for attempt in 0..attempts {
            match dial(address).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = error,
            }
            if attempt + 1 < attempts {
                tokio::time::sleep(RETRY_BACKOFF_STEP * u32::try_from(attempt + 1).unwrap_or(u32::MAX)).await;
            }
        }
        Err(last_error)
    }

    async fn handshake(&self, stream: TcpStream, uri: &Uri) -> io::Result<Upstream> {
        let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
        handshake_with(&self.tls, stream, host)
            .await
            .map(|tls| Upstream::Tls(Box::new(tls)))
    }
}

async fn handshake_with<Io>(tls: &TlsConnector, stream: Io, host: &str) -> io::Result<TlsStream<Io>>
where
    Io: AsyncRead + AsyncWrite + Unpin,
{
    let server_name =
        ServerName::try_from(host.to_owned()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, tls.connect(server_name, stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timeout"))?
}

async fn dial(address: &str) -> io::Result<TcpStream> {
    let stream = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(address))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial timeout"))??;
    SockRef::from(&stream).set_tcp_keepalive(&TcpKeepalive::new().with_time(KEEP_ALIVE).with_interval(KEEP_ALIVE))?;
    Ok(stream)
}
