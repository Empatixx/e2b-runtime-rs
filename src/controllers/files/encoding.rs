use std::io;
use std::pin::Pin;

use async_compression::tokio::bufread::GzipDecoder;
use axum::body::Body;
use axum::http::HeaderMap;
use tokio::io::AsyncRead;
use tokio_stream::StreamExt;
use tokio_util::io::StreamReader;

pub const GZIP: &str = "gzip";
pub const IDENTITY: &str = "identity";
const WILDCARD: &str = "*";
const SUPPORTED: &str = "[gzip]";

struct WeightedEncoding {
    encoding: String,
    quality: f64,
}

pub fn decoded_body(headers: &HeaderMap, body: Body) -> Result<Pin<Box<dyn AsyncRead + Send>>, String> {
    let reader = StreamReader::new(body.into_data_stream().map(|chunk| chunk.map_err(io::Error::other)));
    match content_encoding(headers)? {
        GZIP => Ok(Box::pin(GzipDecoder::new(reader))),
        _ => Ok(Box::pin(reader)),
    }
}

fn content_encoding(headers: &HeaderMap) -> Result<&'static str, String> {
    let header = header_value(headers, "content-encoding");
    if header.is_empty() {
        return Ok(IDENTITY);
    }
    match weighted(header).encoding.as_str() {
        IDENTITY => Ok(IDENTITY),
        GZIP => Ok(GZIP),
        _ => Err(format!(
            "unsupported Content-Encoding: {header}, supported: {SUPPORTED}"
        )),
    }
}

pub fn accept_encoding(headers: &HeaderMap) -> Result<&'static str, String> {
    let header = header_value(headers, "accept-encoding");
    if header.is_empty() {
        return Ok(IDENTITY);
    }

    let (mut encodings, identity_rejected) = parse_accept(header);
    encodings.sort_by(|a, b| b.quality.total_cmp(&a.quality));
    for accepted in encodings.iter().filter(|accepted| accepted.quality != 0.0) {
        match accepted.encoding.as_str() {
            IDENTITY => return Ok(IDENTITY),
            WILDCARD if identity_rejected => return Ok(GZIP),
            WILDCARD => return Ok(IDENTITY),
            GZIP => return Ok(GZIP),
            _ => {}
        }
    }

    if identity_rejected {
        return Err(format!("no acceptable encoding found, supported: {SUPPORTED}"));
    }
    Ok(IDENTITY)
}

pub fn identity_acceptable(headers: &HeaderMap) -> bool {
    let header = header_value(headers, "accept-encoding");
    header.is_empty() || !parse_accept(header).1
}

fn parse_accept(header: &str) -> (Vec<WeightedEncoding>, bool) {
    let encodings: Vec<WeightedEncoding> = header.split(',').map(weighted).collect();
    let has = |encoding: &str, rejected: bool| {
        encodings
            .iter()
            .any(|entry| entry.encoding == encoding && (entry.quality == 0.0) == rejected)
    };
    let identity_rejected = has(IDENTITY, true) || (has(WILDCARD, true) && !has(IDENTITY, false));
    (encodings, identity_rejected)
}

fn weighted(value: &str) -> WeightedEncoding {
    let (encoding, params) = value.trim().split_once(';').unwrap_or((value.trim(), ""));
    let quality = params
        .split(';')
        .rev()
        .find_map(|param| {
            let param = param.trim();
            let (name, quality) = param.split_at_checked(2)?;
            name.eq_ignore_ascii_case("q=").then(|| quality.parse::<f64>().ok())?
        })
        .unwrap_or(1.0);
    WeightedEncoding {
        encoding: encoding.trim().to_ascii_lowercase(),
        quality,
    }
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(name: &'static str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn accept(value: &str) -> Result<&'static str, String> {
        accept_encoding(&headers("accept-encoding", value))
    }

    #[test]
    fn accept_encoding_like_go() {
        assert_eq!(accept_encoding(&HeaderMap::new()), Ok(IDENTITY));
        assert_eq!(accept("gzip"), Ok(GZIP));
        assert_eq!(accept("GZIP;q=0.5, identity;q=0.9"), Ok(IDENTITY));
        assert_eq!(accept("br, gzip;q=0.1"), Ok(GZIP));
        assert_eq!(accept("br"), Ok(IDENTITY));
        assert_eq!(accept("*"), Ok(IDENTITY));
        assert_eq!(accept("*, identity;q=0"), Ok(GZIP));
        assert_eq!(accept("gzip;q=0"), Ok(IDENTITY));
        assert_eq!(
            accept("br, identity;q=0"),
            Err("no acceptable encoding found, supported: [gzip]".to_owned())
        );
        assert_eq!(
            accept("br, *;q=0"),
            Err("no acceptable encoding found, supported: [gzip]".to_owned())
        );
    }

    #[test]
    fn identity_acceptability() {
        assert!(identity_acceptable(&HeaderMap::new()));
        assert!(identity_acceptable(&headers("accept-encoding", "gzip")));
        assert!(!identity_acceptable(&headers("accept-encoding", "identity;q=0")));
        assert!(!identity_acceptable(&headers("accept-encoding", "*;q=0")));
        assert!(identity_acceptable(&headers("accept-encoding", "*;q=0, identity")));
    }

    #[test]
    fn content_encoding_like_go() {
        assert_eq!(content_encoding(&HeaderMap::new()), Ok(IDENTITY));
        assert_eq!(content_encoding(&headers("content-encoding", "Gzip")), Ok(GZIP));
        assert_eq!(content_encoding(&headers("content-encoding", "identity")), Ok(IDENTITY));
        assert_eq!(
            content_encoding(&headers("content-encoding", "br")),
            Err("unsupported Content-Encoding: br, supported: [gzip]".to_owned())
        );
    }
}
