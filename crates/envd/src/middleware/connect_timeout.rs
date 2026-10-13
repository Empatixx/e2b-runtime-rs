use connectrpc::interceptor::{
    Next, NextStream, PayloadStream, StreamRequest, StreamResponse, UnaryRequest, UnaryResponse,
};
use connectrpc::{ConnectError, Interceptor, Protocol, RequestContext, async_trait};

const CONNECT_TIMEOUT_HEADER: &str = "connect-timeout-ms";
const GRPC_TIMEOUT_HEADER: &str = "grpc-timeout";
const MAX_CONNECT_DIGITS: usize = 10;
const MAX_GRPC_VALUE: i64 = 99_999_999;
const NANOS_PER_HOUR: i64 = 3_600_000_000_000;

pub struct ConnectTimeout;

#[async_trait]
impl Interceptor for ConnectTimeout {
    async fn intercept_unary(&self, req: UnaryRequest, next: Next<'_>) -> Result<UnaryResponse, ConnectError> {
        unexpired(&req.ctx)?;
        match req.ctx.deadline() {
            Some(deadline) => tokio::time::timeout_at(deadline.into(), next.run(req))
                .await
                .unwrap_or_else(|_| Err(deadline_exceeded())),
            None => next.run(req).await,
        }
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
    match asserted_nanos(ctx)? {
        Some(nanos) if nanos <= 0 => Err(deadline_exceeded()),
        _ => Ok(()),
    }
}

fn deadline_exceeded() -> ConnectError {
    ConnectError::deadline_exceeded("context deadline exceeded")
}

fn asserted_nanos(ctx: &RequestContext) -> Result<Option<i128>, ConnectError> {
    let header = |name: &str| {
        ctx.headers()
            .get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
            .filter(|text| !text.is_empty())
    };
    match ctx.protocol() {
        Some(Protocol::Grpc | Protocol::GrpcWeb) => {
            header(GRPC_TIMEOUT_HEADER).map_or(Ok(None), |text| grpc_nanos(&text))
        }
        _ => header(CONNECT_TIMEOUT_HEADER).map_or(Ok(None), |text| connect_nanos(&text).map(Some)),
    }
    .map_err(ConnectError::invalid_argument)
}

fn connect_nanos(text: &str) -> Result<i128, String> {
    if text.len() > MAX_CONNECT_DIGITS {
        return Err(format!("parse timeout: {} has >10 digits", go_quote(text)));
    }
    text.parse::<i64>()
        .map(|millis| i128::from(millis) * 1_000_000)
        .map_err(|_| {
            format!(
                "parse timeout: strconv.ParseInt: parsing {}: invalid syntax",
                go_quote(text)
            )
        })
}

fn grpc_nanos(text: &str) -> Result<Option<i128>, String> {
    let unit = text.as_bytes()[text.len() - 1];
    let nanos_per_unit = grpc_unit(unit)?;
    let value = text[..text.len() - 1]
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
        .ok_or_else(|| format!("protocol error: invalid timeout {}", go_quote(text)))?;
    match value {
        value if value > MAX_GRPC_VALUE => Err(format!("protocol error: timeout {} is too long", go_quote(text))),
        value if unit == b'H' && value > i64::MAX / NANOS_PER_HOUR => Ok(None),
        value => Ok(Some(i128::from(value) * i128::from(nanos_per_unit))),
    }
}

fn grpc_unit(unit: u8) -> Result<i64, String> {
    match unit {
        b'n' => Ok(1),
        b'u' => Ok(1_000),
        b'm' => Ok(1_000_000),
        b'S' => Ok(1_000_000_000),
        b'M' => Ok(60_000_000_000),
        b'H' => Ok(NANOS_PER_HOUR),
        other => Err(format!(
            "protocol error: timeout has invalid unit {}",
            go_quote_rune(other)
        )),
    }
}

fn go_quote_rune(byte: u8) -> String {
    let escaped = match char::from(byte) {
        '\'' => "\\'".to_owned(),
        '\\' => "\\\\".to_owned(),
        '\t' => "\\t".to_owned(),
        c if c.is_ascii_control() => format!("\\x{byte:02x}"),
        c if c.is_control() => format!("\\u{byte:04x}"),
        c => c.to_string(),
    };
    format!("'{escaped}'")
}

fn go_quote(text: &str) -> String {
    let escaped: String = text
        .chars()
        .map(|c| match c {
            '"' => "\\\"".to_owned(),
            '\\' => "\\\\".to_owned(),
            '\t' => "\\t".to_owned(),
            c if c.is_ascii_control() => format!("\\x{:02x}", c as u32),
            c => c.to_string(),
        })
        .collect();
    format!("\"{escaped}\"")
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
        asserted_nanos(&RequestContext::new(headers).with_protocol(Some(protocol)))
            .map_err(|e| e.message.unwrap_or_default())
    }

    fn connect(value: &str) -> Result<Option<i128>, String> {
        nanos_for(Protocol::Connect, CONNECT_TIMEOUT_HEADER, value)
    }

    fn grpc(value: &str) -> Result<Option<i128>, String> {
        nanos_for(Protocol::Grpc, GRPC_TIMEOUT_HEADER, value)
    }

    #[test]
    fn connect_timeouts_parse_like_connect_go() {
        assert_eq!(connect(""), Ok(None));
        assert_eq!(connect("1500"), Ok(Some(1_500_000_000)));
        assert_eq!(connect("-1"), Ok(Some(-1_000_000)));
        assert_eq!(connect("+7"), Ok(Some(7_000_000)));
        assert_eq!(
            connect("abc"),
            Err("parse timeout: strconv.ParseInt: parsing \"abc\": invalid syntax".to_owned())
        );
        assert_eq!(
            connect("12345678901"),
            Err("parse timeout: \"12345678901\" has >10 digits".to_owned())
        );
    }

    #[test]
    fn grpc_timeouts_parse_like_connect_go() {
        assert_eq!(grpc("5S"), Ok(Some(5_000_000_000)));
        assert_eq!(grpc("0n"), Ok(Some(0)));
        assert_eq!(grpc("99999999H"), Ok(None));
        assert_eq!(
            grpc("5x"),
            Err("protocol error: timeout has invalid unit 'x'".to_owned())
        );
        assert_eq!(
            grpc("5"),
            Err("protocol error: timeout has invalid unit '5'".to_owned())
        );
        assert_eq!(grpc("m"), Err("protocol error: invalid timeout \"m\"".to_owned()));
        assert_eq!(grpc("-5m"), Err("protocol error: invalid timeout \"-5m\"".to_owned()));
        assert_eq!(
            grpc("123456789m"),
            Err("protocol error: timeout \"123456789m\" is too long".to_owned())
        );
    }

    #[test]
    fn each_protocol_reads_only_its_own_header() {
        assert_eq!(nanos_for(Protocol::Grpc, CONNECT_TIMEOUT_HEADER, "abc"), Ok(None));
        assert_eq!(nanos_for(Protocol::Connect, GRPC_TIMEOUT_HEADER, "5x"), Ok(None));
    }
}
