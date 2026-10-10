use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use serde::Serialize;

use super::body::{BoxError, ProxyBody};

const CONNECT_STREAM_CONTENT_TYPE: &str = "application/connect+";
const CONNECT_FLAG_END_STREAM: u8 = 0b0000_0010;
const ENVELOPE_PREFIX_LEN: usize = 5;
const CONNECT_CODE_UNAVAILABLE: &str = "unavailable";

#[derive(Serialize)]
struct EndStream {
    error: EndStreamError,
}

#[derive(Serialize)]
struct EndStreamError {
    code: &'static str,
    message: String,
}

#[derive(Debug, Default)]
pub struct EnvelopeTracker {
    prefix: [u8; ENVELOPE_PREFIX_LEN],
    prefix_len: usize,
    remaining: u64,
}

impl EnvelopeTracker {
    pub fn advance(&mut self, mut bytes: &[u8]) {
        loop {
            if self.prefix_len == ENVELOPE_PREFIX_LEN && self.remaining == 0 {
                self.prefix_len = 0;
            }
            if bytes.is_empty() {
                return;
            }
            bytes = match self.prefix_len < ENVELOPE_PREFIX_LEN {
                true => self.read_prefix(bytes),
                false => self.skip_payload(bytes),
            };
        }
    }

    pub fn at_boundary(&self) -> bool {
        self.prefix_len == 0 && self.remaining == 0
    }

    fn read_prefix<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let read = bytes.len().min(ENVELOPE_PREFIX_LEN - self.prefix_len);
        self.prefix[self.prefix_len..self.prefix_len + read].copy_from_slice(&bytes[..read]);
        self.prefix_len += read;
        if self.prefix_len == ENVELOPE_PREFIX_LEN {
            self.remaining = u64::from(u32::from_be_bytes([
                self.prefix[1],
                self.prefix[2],
                self.prefix[3],
                self.prefix[4],
            ]));
        }
        &bytes[read..]
    }

    fn skip_payload<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let read = usize::try_from(self.remaining).map_or(bytes.len(), |remaining| remaining.min(bytes.len()));
        self.remaining -= read as u64;
        &bytes[read..]
    }
}

pub fn is_connect_stream(content_type: &str) -> bool {
    content_type.starts_with(CONNECT_STREAM_CONTENT_TYPE)
}

pub fn connect_end_stream_frame(message: &str) -> Option<Vec<u8>> {
    let body = serde_json::to_vec(&EndStream {
        error: EndStreamError {
            code: CONNECT_CODE_UNAVAILABLE,
            message: message.to_owned(),
        },
    })
    .ok()?;
    let length = u32::try_from(body.len()).ok()?;
    Some(
        std::iter::once(CONNECT_FLAG_END_STREAM)
            .chain(length.to_be_bytes())
            .chain(body)
            .collect(),
    )
}

pub fn ends_streams_on_abort(status: StatusCode, headers: &HeaderMap) -> bool {
    status == StatusCode::OK
        && headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(is_connect_stream)
}

pub struct StreamEndingBody {
    inner: ProxyBody,
    tracker: EnvelopeTracker,
    message: String,
    finished: bool,
}

impl StreamEndingBody {
    pub fn wrap(inner: ProxyBody, message: String) -> ProxyBody {
        Self {
            inner,
            tracker: EnvelopeTracker::default(),
            message,
            finished: false,
        }
        .boxed_unsync()
    }

    fn end_frame(&mut self) -> Option<Bytes> {
        self.tracker
            .at_boundary()
            .then(|| connect_end_stream_frame(&self.message))
            .flatten()
            .map(Bytes::from)
    }
}

impl Body for StreamEndingBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.tracker.advance(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => match self.end_frame() {
                Some(end_frame) => {
                    self.finished = true;
                    Poll::Ready(Some(Ok(Frame::data(end_frame))))
                }
                None => Poll::Ready(Some(Err(error))),
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_envelope(payload: &str) -> Vec<u8> {
        let length = u32::try_from(payload.len()).unwrap();
        [&[0u8][..], &length.to_be_bytes(), payload.as_bytes()].concat()
    }

    fn at_boundary_after(writes: &[Vec<u8>]) -> bool {
        let mut tracker = EnvelopeTracker::default();
        writes.iter().for_each(|write| tracker.advance(write));
        tracker.at_boundary()
    }

    #[test]
    fn envelope_tracker_finds_frame_boundaries() {
        let one = data_envelope(r#"{"a":1}"#);
        let cases: [(&str, Vec<Vec<u8>>, bool); 9] = [
            ("nothing written", vec![], true),
            ("one whole frame", vec![data_envelope("{}")], true),
            ("two whole frames", vec![one.clone(), data_envelope("{}")], true),
            ("empty payload", vec![data_envelope("")], true),
            ("partial prefix", vec![vec![0, 0, 0]], false),
            (
                "prefix only",
                vec![data_envelope("{}")[..ENVELOPE_PREFIX_LEN].to_vec()],
                false,
            ),
            ("partial payload", vec![one[..ENVELOPE_PREFIX_LEN + 2].to_vec()], false),
            (
                "frame split across writes",
                vec![one[..3].to_vec(), one[3..].to_vec()],
                true,
            ),
            (
                "whole frame then partial",
                vec![data_envelope("{}"), one[..6].to_vec()],
                false,
            ),
        ];
        for (name, writes, expected) in cases {
            assert_eq!(at_boundary_after(&writes), expected, "{name}");
        }
    }

    #[test]
    fn end_frame_carries_the_unavailable_error() {
        let message = "the connection to sandbox sbx-1 ended before the stream completed";
        let frame = connect_end_stream_frame(message).unwrap();
        assert_eq!(frame[0], CONNECT_FLAG_END_STREAM);
        let length = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(length, frame.len() - ENVELOPE_PREFIX_LEN);
        let body: serde_json::Value = serde_json::from_slice(&frame[ENVELOPE_PREFIX_LEN..]).unwrap();
        assert_eq!(body["error"]["code"], CONNECT_CODE_UNAVAILABLE);
        assert_eq!(body["error"]["message"], message);
    }

    #[test]
    fn recognises_terminable_framings() {
        let cases = [
            ("application/connect+proto", true),
            ("application/connect+json", true),
            ("application/grpc-web", false),
            ("application/grpc-web+proto", false),
            ("application/grpc-web-text", false),
            ("application/grpc", false),
            ("application/proto", false),
            ("application/json", false),
            ("text/event-stream", false),
            ("text/plain", false),
            ("", false),
        ];
        for (content_type, terminable) in cases {
            assert_eq!(is_connect_stream(content_type), terminable, "{content_type}");
        }
    }

    #[test]
    fn only_a_successful_connect_response_may_end_its_stream() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/connect+json".parse().unwrap());
        assert!(ends_streams_on_abort(StatusCode::OK, &headers));
        assert!(!ends_streams_on_abort(StatusCode::BAD_GATEWAY, &headers));
        headers.insert("content-type", "text/plain".parse().unwrap());
        assert!(!ends_streams_on_abort(StatusCode::OK, &headers));
    }

    #[tokio::test]
    async fn ends_the_stream_at_a_frame_boundary_and_refuses_mid_frame() {
        let broken = |written: Vec<u8>| {
            let chunks = vec![
                Ok::<_, BoxError>(Frame::data(Bytes::from(written))),
                Err::<Frame<Bytes>, BoxError>("reset".into()),
            ];
            http_body_util::StreamBody::new(futures::stream::iter(chunks)).boxed_unsync()
        };
        let whole = StreamEndingBody::wrap(broken(data_envelope("{}")), "ended".to_owned());
        let collected = whole.collect().await.unwrap().to_bytes();
        assert!(collected.len() > data_envelope("{}").len());
        assert_eq!(collected[data_envelope("{}").len()], CONNECT_FLAG_END_STREAM);

        let partial = StreamEndingBody::wrap(broken(data_envelope("{}")[..6].to_vec()), "ended".to_owned());
        assert!(partial.collect().await.is_err());
    }
}
