use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, SizeHint};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;

pub fn full_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes).map_err(|never| match never {}).boxed_unsync()
}

pub fn empty_body() -> ProxyBody {
    full_body(Bytes::new())
}

pub struct HeldBody<Held> {
    inner: ProxyBody,
    held: Option<Held>,
}

impl<Held: Send + Unpin + 'static> HeldBody<Held> {
    pub fn wrap(inner: ProxyBody, held: Held) -> ProxyBody {
        Self {
            inner,
            held: Some(held),
        }
        .boxed_unsync()
    }
}

impl<Held: Unpin> Body for HeldBody<Held> {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(polled, Poll::Ready(None | Some(Err(_)))) {
            self.held = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
pub async fn read_text(response: hyper::Response<ProxyBody>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}
