use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderValue};
use axum::http::{StatusCode, response::Parts};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};

use super::body::{BoxError, ProxyBody};
use crate::go_sniff::detect_content_type;

const SNIFF_LEN: usize = 512;

pub fn needs_sniffing(status: StatusCode, headers: &HeaderMap) -> bool {
    body_allowed(status)
        && !headers.contains_key(CONTENT_TYPE)
        && headers.get(CONTENT_ENCODING).is_none_or(|encoding| encoding.is_empty())
        && headers
            .get(CONTENT_LENGTH)
            .and_then(|length| length.to_str().ok())
            .and_then(|length| length.parse::<u64>().ok())
            .is_some_and(|length| length > 0)
}

fn body_allowed(status: StatusCode) -> bool {
    !(status.is_informational() || status == StatusCode::NO_CONTENT || status == StatusCode::NOT_MODIFIED)
}

pub async fn sniff(parts: &mut Parts, mut body: ProxyBody) -> ProxyBody {
    let mut head = Head::default();
    while head.data.len() < SNIFF_LEN && head.take_next(&mut body).await {}
    if !head.data.is_empty() {
        let detected = HeaderValue::from_static(detect_content_type(&head.data));
        parts.headers.insert(CONTENT_TYPE, detected);
    }
    Replayed {
        frames: head.frames,
        failure: head.failure,
        rest: body,
    }
    .boxed_unsync()
}

#[derive(Default)]
struct Head {
    frames: VecDeque<Frame<Bytes>>,
    data: Vec<u8>,
    failure: Option<BoxError>,
}

impl Head {
    async fn take_next(&mut self, body: &mut ProxyBody) -> bool {
        match body.frame().await {
            Some(Ok(frame)) => {
                let keeps_reading = frame.is_data();
                if let Some(bytes) = frame.data_ref() {
                    self.data
                        .extend_from_slice(&bytes[..bytes.len().min(SNIFF_LEN - self.data.len())]);
                }
                self.frames.push_back(frame);
                keeps_reading
            }
            Some(Err(error)) => {
                self.failure = Some(error);
                false
            }
            None => false,
        }
    }
}

struct Replayed {
    frames: VecDeque<Frame<Bytes>>,
    failure: Option<BoxError>,
    rest: ProxyBody,
}

impl Body for Replayed {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(frame) = self.frames.pop_front() {
            return Poll::Ready(Some(Ok(frame)));
        }
        if let Some(failure) = self.failure.take() {
            return Poll::Ready(Some(Err(failure)));
        }
        Pin::new(&mut self.rest).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.frames.is_empty() && self.failure.is_none() && self.rest.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let buffered = self
            .frames
            .iter()
            .filter_map(Frame::data_ref)
            .map(|bytes| bytes.len() as u64)
            .sum::<u64>();
        let rest = self.rest.size_hint();
        let mut hint = SizeHint::new();
        hint.set_lower(rest.lower() + buffered);
        if let Some(upper) = rest.upper() {
            hint.set_upper(upper + buffered);
        }
        hint
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Response;
    use http_body_util::StreamBody;

    use super::*;
    use crate::proxy::body::full_body;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn sniffs_only_bodies_with_a_length_and_no_declared_type() {
        let length = [("content-length", "5")];
        assert!(needs_sniffing(StatusCode::OK, &headers(&length)));
        assert!(!needs_sniffing(StatusCode::OK, &headers(&[("content-length", "0")])));
        assert!(!needs_sniffing(StatusCode::OK, &headers(&[])));
        assert!(!needs_sniffing(
            StatusCode::OK,
            &headers(&[("content-length", "5"), ("content-type", "")])
        ));
        assert!(!needs_sniffing(
            StatusCode::OK,
            &headers(&[("content-length", "5"), ("content-encoding", "gzip")])
        ));
        assert!(needs_sniffing(
            StatusCode::OK,
            &headers(&[("content-length", "5"), ("content-encoding", "")])
        ));
        assert!(!needs_sniffing(StatusCode::NO_CONTENT, &headers(&length)));
        assert!(!needs_sniffing(StatusCode::NOT_MODIFIED, &headers(&length)));
    }

    #[tokio::test]
    async fn sets_the_detected_type_and_keeps_the_body() {
        let (mut parts, _) = Response::new(()).into_parts();
        let body = sniff(&mut parts, full_body(Bytes::from("<html>hi</html>"))).await;
        assert_eq!(parts.headers[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(body.collect().await.unwrap().to_bytes(), "<html>hi</html>");
    }

    #[tokio::test]
    async fn gathers_small_frames_before_deciding() {
        let (mut parts, _) = Response::new(()).into_parts();
        let frames = ["<ht", "ml>", "hi"].map(|chunk| Ok::<_, BoxError>(Frame::data(Bytes::from(chunk))));
        let body = StreamBody::new(futures::stream::iter(frames)).boxed_unsync();
        let body = sniff(&mut parts, body).await;
        assert_eq!(parts.headers[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(body.collect().await.unwrap().to_bytes(), "<html>hi");
    }

    #[tokio::test]
    async fn keeps_the_size_hint_of_the_whole_body() {
        let (mut parts, _) = Response::new(()).into_parts();
        let body = sniff(&mut parts, full_body(Bytes::from("<html>hi</html>"))).await;
        assert_eq!(body.size_hint().exact(), Some(15));
    }

    #[tokio::test]
    async fn leaves_the_type_unset_for_an_empty_body() {
        let (mut parts, _) = Response::new(()).into_parts();
        sniff(&mut parts, full_body(Bytes::new())).await;
        assert!(parts.headers.get(CONTENT_TYPE).is_none());
    }

    #[tokio::test]
    async fn replays_a_failure_after_the_buffered_frames() {
        let (mut parts, _) = Response::new(()).into_parts();
        let frames = vec![
            Ok::<_, BoxError>(Frame::data(Bytes::from("abc"))),
            Err::<Frame<Bytes>, BoxError>("reset".into()),
        ];
        let body = StreamBody::new(futures::stream::iter(frames)).boxed_unsync();
        let mut body = sniff(&mut parts, body).await;
        assert_eq!(parts.headers[CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(body.frame().await.unwrap().unwrap().into_data().unwrap(), "abc");
        assert!(body.frame().await.unwrap().is_err());
    }
}
