use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::task::{Context, Poll};

use hyper_util::client::legacy::connect::{Connected, Connection};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::go_replies::Replacement;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

#[derive(Clone, Default)]
pub struct Connections {
    active: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    next_id: Arc<AtomicU64>,
    current: Arc<AtomicI64>,
}

impl Connections {
    pub fn sharing_counter(current: Arc<AtomicI64>) -> Self {
        Self {
            current,
            ..Self::default()
        }
    }

    pub fn track<Io: Abortable>(&self, io: Io, reset_on_abort: bool) -> Tracked<Io> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();
        self.active.lock().insert(id, token.clone());
        self.current.fetch_add(1, Ordering::SeqCst);
        Tracked {
            io,
            aborted: Box::pin(token.cancelled_owned()),
            reset_on_abort,
            was_aborted: false,
            probe: None,
            replacement: None,
            _membership: Membership {
                id,
                connections: self.clone(),
            },
        }
    }

    pub fn track_probed<Io: Abortable>(&self, io: Io, probe: Arc<Probe>) -> Tracked<Io> {
        let mut tracked = self.track(io, false);
        tracked.probe = Some(probe);
        tracked
    }

    pub fn abort_all(&self) {
        let aborted = self.active.lock().drain().map(|(_, token)| token).collect::<Vec<_>>();
        self.current.fetch_sub(aborted.len() as i64, Ordering::SeqCst);
        aborted.iter().for_each(CancellationToken::cancel);
    }

    pub fn current(&self) -> i64 {
        self.current.load(Ordering::SeqCst)
    }
}

const H2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_PREFACE_REQUEST_LINE: usize = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Unknown,
    Http1,
    H2c,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeState {
    pub protocol: Protocol,
    pub preface_pending: bool,
}

pub struct Probe {
    seen: Mutex<Vec<u8>>,
    state: watch::Sender<ProbeState>,
}

impl Probe {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            state: watch::channel(ProbeState {
                protocol: Protocol::Unknown,
                preface_pending: false,
            })
            .0,
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<ProbeState> {
        self.state.subscribe()
    }

    fn record(&self, data: &[u8]) {
        let mut seen = self.seen.lock();
        if self.state.borrow().protocol != Protocol::Unknown {
            return;
        }
        let room = H2_PREFACE.len() - seen.len();
        seen.extend_from_slice(&data[..data.len().min(room)]);
        let matches = seen.iter().zip(H2_PREFACE).all(|(seen, expected)| seen == expected);
        let state = match (matches, seen.len() == H2_PREFACE.len()) {
            (false, _) => ProbeState {
                protocol: Protocol::Http1,
                preface_pending: false,
            },
            (true, true) => ProbeState {
                protocol: Protocol::H2c,
                preface_pending: false,
            },
            (true, false) => ProbeState {
                protocol: Protocol::Unknown,
                preface_pending: seen.len() >= H2_PREFACE_REQUEST_LINE,
            },
        };
        self.state.send_replace(state);
    }
}

pub trait Abortable {
    fn reset_on_close(&self);

    fn proxied(&self) -> bool {
        false
    }
}

impl Abortable for TcpStream {
    fn reset_on_close(&self) {
        drop(self.set_zero_linger());
    }
}

struct Membership {
    id: u64,
    connections: Connections,
}

impl Drop for Membership {
    fn drop(&mut self) {
        if self.connections.active.lock().remove(&self.id).is_some() {
            self.connections.current.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

pub struct Tracked<Io> {
    io: Io,
    aborted: Pin<Box<WaitForCancellationFutureOwned>>,
    reset_on_abort: bool,
    was_aborted: bool,
    probe: Option<Arc<Probe>>,
    replacement: Option<Replacement>,
    _membership: Membership,
}

impl<Io: Abortable> Tracked<Io> {
    fn poll_aborted(&mut self, cx: &mut Context<'_>) -> Poll<io::Error> {
        self.aborted.as_mut().poll(cx).map(|()| {
            if self.reset_on_abort && !self.was_aborted {
                self.io.reset_on_close();
            }
            self.was_aborted = true;
            io::Error::new(io::ErrorKind::ConnectionAborted, "connection closed by the proxy")
        })
    }
}

impl<Io: AsyncRead + Abortable + Unpin> AsyncRead for Tracked<Io> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.poll_aborted(cx) {
            Poll::Ready(error) => Poll::Ready(Err(error)),
            Poll::Pending => {
                let before = buf.filled().len();
                let polled = Pin::new(&mut self.io).poll_read(cx, buf);
                if let (Poll::Ready(Ok(())), Some(probe)) = (&polled, &self.probe) {
                    probe.record(&buf.filled()[before..]);
                }
                polled
            }
        }
    }
}

impl<Io: AsyncWrite + Abortable + Unpin> Tracked<Io> {
    fn write_through(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if self.probe.is_some() && self.replacement.is_none() {
            self.replacement = Replacement::of(buf);
        }
        let Some(replacement) = self.replacement.as_mut() else {
            return Pin::new(&mut self.io).poll_write(cx, buf);
        };
        loop {
            let written = match Pin::new(&mut self.io).poll_write(cx, replacement.remaining()) {
                Poll::Ready(Ok(written)) => written,
                other => return other,
            };
            if let Some(consumed) = replacement.advance(written) {
                self.replacement = None;
                return Poll::Ready(Ok(consumed));
            }
        }
    }
}

impl<Io: AsyncWrite + Abortable + Unpin> AsyncWrite for Tracked<Io> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.poll_aborted(cx) {
            Poll::Ready(error) => Poll::Ready(Err(error)),
            Poll::Pending => self.write_through(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.was_aborted || self.poll_aborted(cx).is_ready() {
            true => Poll::Ready(Ok(())),
            false => Pin::new(&mut self.io).poll_shutdown(cx),
        }
    }
}

impl<Io: Abortable> Connection for Tracked<Io> {
    fn connected(&self) -> Connected {
        Connected::new().proxy(self.io.proxied())
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn tracked_pair(connections: &Connections) -> (Tracked<TcpStream>, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (connections.track(client, true), server)
    }

    fn state_after(chunks: &[&[u8]]) -> ProbeState {
        let probe = Probe::new();
        chunks.iter().for_each(|chunk| probe.record(chunk));
        *probe.subscribe().borrow()
    }

    #[test]
    fn the_probe_tells_h2c_from_http1() {
        let preface = |protocol, preface_pending| ProbeState {
            protocol,
            preface_pending,
        };
        assert_eq!(state_after(&[b"GET / HTTP/1.1\r\n"]), preface(Protocol::Http1, false));
        assert_eq!(
            state_after(&[b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"]),
            preface(Protocol::H2c, false)
        );
        assert_eq!(
            state_after(&[b"PRI * HTTP/2.0", b"\r\n\r\n"]),
            preface(Protocol::Unknown, true)
        );
        assert_eq!(state_after(&[b"PRI * HT"]), preface(Protocol::Unknown, false));
        assert_eq!(
            state_after(&[b"PRI * HTTP/2.0\r\n\r\nS"]),
            preface(Protocol::Unknown, true)
        );
        assert_eq!(
            state_after(&[b"PRI * HTTP/2.0\r\n\r\nBADBAD"]),
            preface(Protocol::Http1, false)
        );
        assert_eq!(state_after(&[]), preface(Protocol::Unknown, false));
    }

    #[tokio::test]
    async fn counts_live_connections_until_dropped() {
        let connections = Connections::default();
        let (tracked, _server) = tracked_pair(&connections).await;
        assert_eq!(connections.current(), 1);
        drop(tracked);
        assert_eq!(connections.current(), 0);
    }

    #[tokio::test]
    async fn shared_counter_spans_connection_sets() {
        let counter = Arc::new(AtomicI64::new(0));
        let first = Connections::sharing_counter(counter.clone());
        let second = Connections::sharing_counter(counter);
        let (_a, _server_a) = tracked_pair(&first).await;
        let (_b, _server_b) = tracked_pair(&second).await;
        assert_eq!(first.current(), 2);
        assert_eq!(second.current(), 2);
    }

    #[tokio::test]
    async fn abort_all_resets_the_peer() {
        let connections = Connections::default();
        let (mut tracked, mut server) = tracked_pair(&connections).await;
        connections.abort_all();
        assert!(tracked.read_u8().await.is_err());
        drop(tracked);
        let error = server.read_u8().await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
        server.write_all(b"x").await.ok();
    }
}
