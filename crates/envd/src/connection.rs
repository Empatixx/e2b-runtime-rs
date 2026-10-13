use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::serve::Listener;
use bytes::{Buf, Bytes};
use http::{HeaderValue, StatusCode, header};
use hyper::ext::ReasonPhrase;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const MAX_HEADER_BYTES: usize = (1 << 20) + 4096;
const MAX_HEADERS: usize = MAX_HEADER_BYTES / SHORTEST_HEADER_LINE.len();
const IDLE_TIMEOUT: Duration = Duration::from_secs(640);

const SHORTEST_HEADER_LINE: &[u8] = b"a:\r\n";
const STATUS_LINE_START: &[u8] = b"HTTP/1.1 4";
const LONGEST_ERROR_HEAD: usize = 4096;
const HYPER_ERRORS: [&str; 2] = ["400 Bad Request", "431 Request Header Fields Too Large"];
const HYPER_ERROR_FIELDS: &str = "connection: close\r\ncontent-length: 0\r\ndate: ";
const HTTP_DATE_AND_END: usize = "Tue, 13 Oct 2026 10:00:00 GMT\r\n\r\n".len();
const EMPTY_BODY: &str = "content-length: 0";
const GO_ERROR_HEADERS: &str = "\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n";

pub async fn serve(mut listener: impl Listener, app: Router) -> ! {
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .max_buf_size(MAX_HEADER_BYTES)
        .max_headers(MAX_HEADERS)
        .timer(TokioTimer::new())
        .header_read_timeout(IDLE_TIMEOUT);
    builder.http2().enable_connect_protocol();
    loop {
        let (stream, _) = listener.accept().await;
        let builder = builder.clone();
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            builder
                .serve_connection_with_upgrades(TokioIo::new(GoErrorPages::new(stream)), service)
                .await
                .unwrap_or_else(|e| tracing::trace!(error = %e, "failed to serve connection"))
        });
    }
}

pub fn go_status_error(status: StatusCode, text: &str) -> Response {
    let reason = format!("{}: {text}", status.canonical_reason().unwrap_or_default());
    let mut response = (status, [(header::CONNECTION, HeaderValue::from_static("close"))]).into_response();
    if let Ok(reason) = ReasonPhrase::try_from(reason) {
        response.extensions_mut().insert(reason);
    }
    response
}

pub struct GoErrorPages<S> {
    inner: S,
    pending: Bytes,
}

impl<S> GoErrorPages<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            pending: Bytes::new(),
        }
    }
}

impl<S: AsyncWrite + Unpin> GoErrorPages<S> {
    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.pending.has_remaining() {
            let written = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.pending))?;
            if written == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pending.advance(written);
        }
        Poll::Ready(Ok(()))
    }

    fn replace(&mut self, head: &[u8]) -> Option<usize> {
        go_page_for(head).map(|page| {
            self.pending = page;
            head.len()
        })
    }
}

fn go_page_for(head: &[u8]) -> Option<Bytes> {
    let text = (head.starts_with(STATUS_LINE_START) && head.len() <= LONGEST_ERROR_HEAD)
        .then(|| std::str::from_utf8(head).ok())
        .flatten()?;
    let (status, fields) = text.strip_prefix("HTTP/1.1 ")?.split_once("\r\n")?;
    (is_hyper_error(status, fields) || is_go_status_error(status, fields)).then(|| go_page(status))
}

fn is_hyper_error(status: &str, fields: &str) -> bool {
    HYPER_ERRORS.contains(&status)
        && fields
            .strip_prefix(HYPER_ERROR_FIELDS)
            .is_some_and(|rest| rest.len() == HTTP_DATE_AND_END && rest.ends_with("\r\n\r\n"))
}

fn is_go_status_error(status: &str, fields: &str) -> bool {
    let has_empty_body = fields.ends_with("\r\n\r\n") && fields.split("\r\n").any(|field| field == EMPTY_BODY);
    has_empty_body && status.split_once(": ").is_some_and(|(plain, _)| is_canonical(plain))
}

fn is_canonical(status: &str) -> bool {
    status
        .split_once(' ')
        .and_then(|(code, reason)| StatusCode::from_bytes(code.as_bytes()).ok().map(|code| (code, reason)))
        .is_some_and(|(code, reason)| code.canonical_reason() == Some(reason))
}

fn go_page(status: &str) -> Bytes {
    Bytes::from(format!("HTTP/1.1 {status}{GO_ERROR_HEADERS}{status}"))
}

impl<S: AsyncRead + Unpin> AsyncRead for GoErrorPages<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for GoErrorPages<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        ready!(self.poll_pending(cx))?;
        match self.replace(buf) {
            Some(consumed) => Poll::Ready(Ok(consumed)),
            None => Pin::new(&mut self.inner).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        ready!(self.poll_pending(cx))?;
        match bufs.first().and_then(|first| self.replace(first)) {
            Some(consumed) => Poll::Ready(Ok(consumed)),
            None => Pin::new(&mut self.inner).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_pending(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_pending(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    async fn written_by(write: &[u8]) -> Vec<u8> {
        let mut pages = GoErrorPages::new(Vec::new());
        pages.write_all(write).await.unwrap();
        pages.flush().await.unwrap();
        pages.inner
    }

    const DATE: &str = "date: Tue, 13 Oct 2026 10:00:00 GMT\r\n\r\n";

    fn go(status: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n{status}")
            .into_bytes()
    }

    #[tokio::test]
    async fn the_hyper_431_becomes_the_go_page() {
        let hyper =
            format!("HTTP/1.1 431 Request Header Fields Too Large\r\nconnection: close\r\ncontent-length: 0\r\n{DATE}");
        assert_eq!(
            written_by(hyper.as_bytes()).await,
            go("431 Request Header Fields Too Large")
        );
    }

    #[tokio::test]
    async fn the_hyper_400_becomes_the_go_page() {
        let hyper = format!("HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n{DATE}");
        assert_eq!(written_by(hyper.as_bytes()).await, go("400 Bad Request"));
    }

    #[tokio::test]
    async fn a_status_error_with_a_reason_becomes_the_go_page() {
        let ours = format!(
            "HTTP/1.1 400 Bad Request: missing required Host header\r\nconnection: close\r\ncontent-length: 0\r\nvary: origin\r\n{DATE}"
        );
        assert_eq!(
            written_by(ours.as_bytes()).await,
            go("400 Bad Request: missing required Host header")
        );
    }

    #[tokio::test]
    async fn other_writes_pass_through() {
        let responses = [
            "HTTP/1.1 431 Request Header Fields Too Large\r\ncontent-length: 2\r\n\r\nok".to_owned(),
            format!("HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n{DATE}"),
            format!("HTTP/1.1 400 Bad: thing\r\ncontent-length: 0\r\n{DATE}"),
            format!("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n{DATE}"),
        ];
        for response in responses {
            assert_eq!(written_by(response.as_bytes()).await, response.as_bytes());
        }
    }
}
