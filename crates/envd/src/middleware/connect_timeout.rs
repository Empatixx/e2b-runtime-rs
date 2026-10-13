use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::middleware::Next as AxumNext;
use axum::response::Response;
use connectrpc::interceptor::{
    Next, NextStream, PayloadStream, StreamRequest, StreamResponse, UnaryRequest, UnaryResponse,
};
use connectrpc::{ConnectError, Interceptor, Protocol, RequestContext, Spec, async_trait};
use http::{HeaderMap, HeaderName};

use crate::proto::filesystem::FILESYSTEM_WATCH_DIR_SPEC;
use crate::proto::process::{PROCESS_CONNECT_SPEC, PROCESS_START_SPEC, PROCESS_STREAM_INPUT_SPEC};

const CONNECT_TIMEOUT_HEADER: &str = "connect-timeout-ms";
const GRPC_TIMEOUT_HEADER: &str = "grpc-timeout";
const MAX_CONNECT_DIGITS: usize = 10;
const MAX_GRPC_VALUE: i64 = 99_999_999;
const NANOS_PER_HOUR: i64 = 3_600_000_000_000;
const STREAMING: [Spec; 4] = [
    PROCESS_CONNECT_SPEC,
    PROCESS_START_SPEC,
    PROCESS_STREAM_INPUT_SPEC,
    FILESYSTEM_WATCH_DIR_SPEC,
];

pub struct ConnectTimeout;

#[derive(Clone)]
struct HeldTimeout {
    headers: HeaderMap,
    arrived: Instant,
}

impl HeldTimeout {
    fn take_from(headers: &mut HeaderMap) -> Self {
        Self {
            headers: [CONNECT_TIMEOUT_HEADER, GRPC_TIMEOUT_HEADER]
                .into_iter()
                .filter_map(|name| headers.remove(name).map(|value| (HeaderName::from_static(name), value)))
                .collect(),
            arrived: Instant::now(),
        }
    }

    fn unexpired(&self, protocol: Option<Protocol>) -> Result<(), ConnectError> {
        match asserted_nanos(&self.headers, protocol)? {
            Some(nanos) if self.is_past(nanos) => Err(deadline_exceeded()),
            _ => Ok(()),
        }
    }

    fn is_past(&self, nanos: i128) -> bool {
        nanos <= 0 || u64::try_from(nanos).is_ok_and(|nanos| self.arrived.elapsed() >= Duration::from_nanos(nanos))
    }
}

pub async fn hold_unary_timeout(mut request: Request, next: AxumNext) -> Response {
    if is_unary(request.uri().path()) {
        let held = HeldTimeout::take_from(request.headers_mut());
        request.extensions_mut().insert(held);
    }
    next.run(request).await
}

fn is_unary(path: &str) -> bool {
    STREAMING.iter().all(|spec| spec.procedure != path)
}

#[async_trait]
impl Interceptor for ConnectTimeout {
    async fn intercept_unary(&self, req: UnaryRequest, next: Next<'_>) -> Result<UnaryResponse, ConnectError> {
        req.ctx
            .extensions()
            .get::<HeldTimeout>()
            .map_or(Ok(()), |held| held.unexpired(req.ctx.protocol()))?;
        next.run(req).await
    }

    async fn intercept_streaming(
        &self,
        req: StreamRequest,
        inbound: PayloadStream,
        next: NextStream<'_>,
    ) -> Result<StreamResponse, ConnectError> {
        unexpired(&req.ctx)?;
        next.run(req, inbound).await
    }
}

fn unexpired(ctx: &RequestContext) -> Result<(), ConnectError> {
    match asserted_nanos(ctx.headers(), ctx.protocol())? {
        Some(nanos) if nanos <= 0 => Err(deadline_exceeded()),
        _ => Ok(()),
    }
}

fn deadline_exceeded() -> ConnectError {
    ConnectError::deadline_exceeded("context deadline exceeded")
}

fn asserted_nanos(headers: &HeaderMap, protocol: Option<Protocol>) -> Result<Option<i128>, ConnectError> {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .filter(|text| !text.is_empty())
    };
    match protocol {
        Some(Protocol::Grpc | Protocol::GrpcWeb) => {
            header(GRPC_TIMEOUT_HEADER).map_or(Ok(None), |text| grpc_nanos(&text))
        }
        _ => header(CONNECT_TIMEOUT_HEADER).map_or(Ok(None), |text| connect_nanos(&text).map(Some)),
    }
    .map_err(ConnectError::invalid_argument)
}

fn connect_nanos(text: &str) -> Result<i128, String> {
    if text.len() > MAX_CONNECT_DIGITS {
        return Err(format!("timeout {text:?} has more than 10 digits"));
    }
    text.parse::<i64>()
        .map(|millis| i128::from(millis) * 1_000_000)
        .map_err(|_| format!("invalid timeout {text:?}"))
}

fn grpc_nanos(text: &str) -> Result<Option<i128>, String> {
    let unit = text.as_bytes()[text.len() - 1];
    let nanos_per_unit = grpc_unit(unit).ok_or_else(|| format!("timeout {text:?} has an invalid unit"))?;
    let value = text[..text.len() - 1]
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
        .ok_or_else(|| format!("invalid timeout {text:?}"))?;
    match value {
        value if value > MAX_GRPC_VALUE => Err(format!("timeout {text:?} is too long")),
        value if unit == b'H' && value > i64::MAX / NANOS_PER_HOUR => Ok(None),
        value => Ok(Some(i128::from(value) * i128::from(nanos_per_unit))),
    }
}

fn grpc_unit(unit: u8) -> Option<i64> {
    match unit {
        b'n' => Some(1),
        b'u' => Some(1_000),
        b'm' => Some(1_000_000),
        b'S' => Some(1_000_000_000),
        b'M' => Some(60_000_000_000),
        b'H' => Some(NANOS_PER_HOUR),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nanos_for(protocol: Protocol, header: &str, value: &str) -> Result<Option<i128>, String> {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::HeaderName::from_bytes(header.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        asserted_nanos(&headers, Some(protocol)).map_err(|e| e.message.unwrap_or_default())
    }

    fn connect(value: &str) -> Result<Option<i128>, String> {
        nanos_for(Protocol::Connect, CONNECT_TIMEOUT_HEADER, value)
    }

    fn grpc(value: &str) -> Result<Option<i128>, String> {
        nanos_for(Protocol::Grpc, GRPC_TIMEOUT_HEADER, value)
    }

    #[test]
    fn connect_timeouts_parse_to_nanos() {
        assert_eq!(connect(""), Ok(None));
        assert_eq!(connect("1500"), Ok(Some(1_500_000_000)));
        assert_eq!(connect("-1"), Ok(Some(-1_000_000)));
        assert_eq!(connect("+7"), Ok(Some(7_000_000)));
        assert_eq!(connect("abc"), Err("invalid timeout \"abc\"".to_owned()));
        assert_eq!(
            connect("12345678901"),
            Err("timeout \"12345678901\" has more than 10 digits".to_owned())
        );
    }

    #[test]
    fn grpc_timeouts_parse_to_nanos() {
        assert_eq!(grpc("5S"), Ok(Some(5_000_000_000)));
        assert_eq!(grpc("0n"), Ok(Some(0)));
        assert_eq!(grpc("99999999H"), Ok(None));
        assert_eq!(grpc("5x"), Err("timeout \"5x\" has an invalid unit".to_owned()));
        assert_eq!(grpc("5"), Err("timeout \"5\" has an invalid unit".to_owned()));
        assert_eq!(grpc("m"), Err("invalid timeout \"m\"".to_owned()));
        assert_eq!(grpc("-5m"), Err("invalid timeout \"-5m\"".to_owned()));
        assert_eq!(grpc("123456789m"), Err("timeout \"123456789m\" is too long".to_owned()));
    }

    #[test]
    fn only_streaming_procedures_keep_their_timeout_header() {
        assert!(is_unary("/filesystem.Filesystem/ListDir"));
        assert!(is_unary("/process.Process/SendInput"));
        assert!(!is_unary("/process.Process/Start"));
        assert!(!is_unary("/filesystem.Filesystem/WatchDir"));
    }

    #[test]
    fn a_held_timeout_expires_only_once_its_time_has_passed() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECT_TIMEOUT_HEADER, "60000".parse().unwrap());
        let held = HeldTimeout::take_from(&mut headers);
        assert!(headers.is_empty());
        assert!(held.unexpired(Some(Protocol::Connect)).is_ok());
        assert!(held.is_past(0));
        assert!(!held.is_past(60_000_000_000));
    }

    #[test]
    fn each_protocol_reads_only_its_own_header() {
        assert_eq!(nanos_for(Protocol::Grpc, CONNECT_TIMEOUT_HEADER, "abc"), Ok(None));
        assert_eq!(nanos_for(Protocol::Connect, GRPC_TIMEOUT_HEADER, "5x"), Ok(None));
    }
}
