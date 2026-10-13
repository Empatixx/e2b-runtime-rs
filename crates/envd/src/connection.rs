use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub const MAX_HEADER_BYTES: usize = (1 << 20) + 4096;
pub const MAX_HEADERS: usize = MAX_HEADER_BYTES / SHORTEST_HEADER_LINE.len();

const SHORTEST_HEADER_LINE: &[u8] = b"a:\r\n";
const HYPER_TOO_LARGE: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\n";
const HYPER_CONNECTION_CLOSE: &[u8] = b"connection: close\r\n";
const HYPER_EMPTY_BODY_DATE: &[u8] = b"content-length: 0\r\ndate: ";
const GO_TOO_LARGE: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n431 Request Header Fields Too Large";
const HTTP_DATE_AND_END: usize = "Tue, 13 Oct 2026 10:00:00 GMT\r\n\r\n".len();

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
        is_hyper_too_large(head).then(|| {
            self.pending = Bytes::from_static(GO_TOO_LARGE);
            head.len()
        })
    }
}

fn is_hyper_too_large(head: &[u8]) -> bool {
    head.strip_prefix(HYPER_TOO_LARGE)
        .map(|rest| rest.strip_prefix(HYPER_CONNECTION_CLOSE).unwrap_or(rest))
        .and_then(|rest| rest.strip_prefix(HYPER_EMPTY_BODY_DATE))
        .is_some_and(|rest| rest.len() == HTTP_DATE_AND_END && rest.ends_with(b"\r\n\r\n"))
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

    #[tokio::test]
    async fn the_hyper_431_becomes_the_go_page() {
        let hyper = b"HTTP/1.1 431 Request Header Fields Too Large\r\ncontent-length: 0\r\ndate: Tue, 13 Oct 2026 10:00:00 GMT\r\n\r\n";
        assert_eq!(written_by(hyper).await, GO_TOO_LARGE);
    }

    #[tokio::test]
    async fn the_closing_hyper_431_becomes_the_go_page() {
        let hyper = b"HTTP/1.1 431 Request Header Fields Too Large\r\nconnection: close\r\ncontent-length: 0\r\ndate: Tue, 13 Oct 2026 10:00:00 GMT\r\n\r\n";
        assert_eq!(written_by(hyper).await, GO_TOO_LARGE);
    }

    #[tokio::test]
    async fn other_writes_pass_through() {
        let response = b"HTTP/1.1 431 Request Header Fields Too Large\r\ncontent-length: 2\r\n\r\nok";
        assert_eq!(written_by(response).await, response);
    }
}
