use std::io::Write;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use bytes::{BufMut, BytesMut};
use flate2::Compression;
use flate2::write::GzEncoder;
use futures::StreamExt;

use crate::util::go_json::escape_html;

const HEADER_LEN: usize = 5;
const COMPRESSED: u8 = 0x01;
const END_STREAM: u8 = 0x02;
const CONNECT_CONTENT_ENCODING: &str = "connect-content-encoding";
const CONNECT_ACCEPT_ENCODING: &str = "connect-accept-encoding";
const GZIP: &str = "gzip";
const IDENTITY: &str = "identity";

pub async fn end_stream_like_go(request: Request, next: Next) -> Response {
    let gzip = negotiates_gzip(request.headers());
    let response = next.run(request).await;
    match is_connect_stream(response.headers()) {
        true => rewritten(response, gzip),
        false => response,
    }
}

fn negotiates_gzip(headers: &HeaderMap) -> bool {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    match text(CONNECT_CONTENT_ENCODING) {
        "" | IDENTITY => text(CONNECT_ACCEPT_ENCODING).split([',', ' ']).any(|name| name == GZIP),
        sent => sent == GZIP,
    }
}

fn is_connect_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|kind| kind.as_bytes().starts_with(b"application/connect+"))
}

fn rewritten(response: Response, gzip: bool) -> Response {
    let (mut parts, body) = response.into_parts();
    if gzip {
        parts
            .headers
            .insert(CONNECT_CONTENT_ENCODING, HeaderValue::from_static(GZIP));
    }
    let mut envelopes = Envelopes {
        gzip,
        ..Envelopes::default()
    };
    let chunks = body
        .into_data_stream()
        .map(move |chunk| chunk.map(|chunk| envelopes.rewrite_end_stream(chunk)));
    Response::from_parts(parts, Body::from_stream(chunks))
}

#[derive(Default)]
struct Envelopes {
    gzip: bool,
    partial_header: Vec<u8>,
    payload_left: usize,
}

struct Frame {
    start: usize,
    flags: u8,
    payload_len: usize,
}

impl Envelopes {
    fn rewrite_end_stream(&mut self, chunk: Bytes) -> Bytes {
        let gzip = self.gzip;
        self.whole_frames_in(&chunk)
            .into_iter()
            .find(|frame| frame.flags == END_STREAM)
            .map(|frame| with_go_payload(&chunk, &frame, gzip))
            .unwrap_or(chunk)
    }

    fn whole_frames_in(&mut self, chunk: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            offset = match self.payload_left {
                0 => self.read_header(chunk, offset, &mut frames),
                left => self.skip_payload(chunk.len() - offset, left) + offset,
            };
        }
        frames
    }

    fn skip_payload(&mut self, available: usize, left: usize) -> usize {
        let skipped = available.min(left);
        self.payload_left -= skipped;
        skipped
    }

    fn read_header(&mut self, chunk: &[u8], offset: usize, frames: &mut Vec<Frame>) -> usize {
        let started_here = self.partial_header.is_empty();
        let taken = (HEADER_LEN - self.partial_header.len()).min(chunk.len() - offset);
        self.partial_header.extend_from_slice(&chunk[offset..offset + taken]);
        let end = offset + taken;
        if self.partial_header.len() == HEADER_LEN {
            let frame = self.finish_header(offset);
            if started_here && end + frame.payload_len <= chunk.len() {
                frames.push(frame);
            }
        }
        end
    }

    fn finish_header(&mut self, start: usize) -> Frame {
        let header = std::mem::take(&mut self.partial_header);
        let payload_len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        self.payload_left = payload_len;
        Frame {
            start,
            flags: header[0],
            payload_len,
        }
    }
}

fn with_go_payload(chunk: &[u8], frame: &Frame, gzip: bool) -> Bytes {
    let payload_start = frame.start + HEADER_LEN;
    let payload_end = payload_start + frame.payload_len;
    let escaped = escape_html(&String::from_utf8_lossy(&chunk[payload_start..payload_end])).into_bytes();
    let (flags, payload) = match gzip {
        true => (frame.flags | COMPRESSED, gzipped(&escaped)),
        false => (frame.flags, escaped),
    };
    let mut rewritten = BytesMut::with_capacity(chunk.len() + payload.len());
    rewritten.put_slice(&chunk[..frame.start]);
    rewritten.put_u8(flags);
    rewritten.put_u32(payload.len() as u32);
    rewritten.put_slice(&payload);
    rewritten.put_slice(&chunk[payload_end..]);
    rewritten.freeze()
}

fn gzipped(payload: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(payload)
        .and_then(|_| encoder.finish())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use axum::http::HeaderName;
    use flate2::read::GzDecoder;

    use super::*;

    fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
        [&[flags][..], &(payload.len() as u32).to_be_bytes(), payload].concat()
    }

    fn rewritten_chunks(gzip: bool, chunks: Vec<Vec<u8>>) -> Vec<u8> {
        let mut envelopes = Envelopes {
            gzip,
            ..Envelopes::default()
        };
        chunks
            .into_iter()
            .flat_map(|chunk| envelopes.rewrite_end_stream(Bytes::from(chunk)).to_vec())
            .collect()
    }

    #[test]
    fn the_end_stream_json_is_escaped_like_go() {
        let end = envelope(END_STREAM, br#"{"error":{"message":"a <b> & c"}}"#);
        assert_eq!(
            rewritten_chunks(false, vec![end]),
            envelope(END_STREAM, br#"{"error":{"message":"a \u003cb\u003e \u0026 c"}}"#)
        );
    }

    #[test]
    fn a_gzipped_stream_gets_a_compressed_end_stream_like_go() {
        let rewritten = rewritten_chunks(true, vec![envelope(END_STREAM, br#"{"m":"<"}"#)]);
        let mut json = String::new();
        GzDecoder::new(&rewritten[HEADER_LEN..])
            .read_to_string(&mut json)
            .unwrap();

        assert_eq!(rewritten[0], END_STREAM | COMPRESSED);
        assert_eq!(json, r#"{"m":"\u003c"}"#);
    }

    #[test]
    fn gzip_is_negotiated_like_connect_go() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            pairs
                .iter()
                .map(|(name, value)| (HeaderName::from_static(name), HeaderValue::from_static(value)))
                .collect::<HeaderMap>()
        };
        assert!(negotiates_gzip(&headers(&[(CONNECT_ACCEPT_ENCODING, "br, gzip")])));
        assert!(negotiates_gzip(&headers(&[(CONNECT_CONTENT_ENCODING, "gzip")])));
        assert!(negotiates_gzip(&headers(&[
            (CONNECT_CONTENT_ENCODING, "identity"),
            (CONNECT_ACCEPT_ENCODING, "gzip")
        ])));
        assert!(!negotiates_gzip(&headers(&[(CONNECT_ACCEPT_ENCODING, "br")])));
        assert!(!negotiates_gzip(&headers(&[])));
    }

    #[test]
    fn messages_are_left_alone() {
        let message = envelope(0, br#"{"text":"<&>"}"#);
        assert_eq!(rewritten_chunks(false, vec![message.clone()]), message);
    }

    #[test]
    fn a_payload_split_across_chunks_that_looks_like_an_end_stream_is_left_alone() {
        let fake_end = envelope(END_STREAM, b"<>");
        let message = envelope(0, &[b"xx".to_vec(), fake_end.clone()].concat());
        let chunks = vec![message[..7].to_vec(), message[7..].to_vec()];
        assert_eq!(rewritten_chunks(false, chunks), message);
    }

    #[test]
    fn the_end_stream_after_a_message_in_the_same_chunk_is_escaped() {
        let message = envelope(0, b"<>");
        let chunk = [message.clone(), envelope(END_STREAM, b"{\"m\":\"<\"}")].concat();
        assert_eq!(
            rewritten_chunks(false, vec![chunk]),
            [message, envelope(END_STREAM, br#"{"m":"\u003c"}"#)].concat()
        );
    }
}
