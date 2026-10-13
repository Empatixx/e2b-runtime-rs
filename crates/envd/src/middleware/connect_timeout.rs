use connectrpc::interceptor::{
    Next, NextStream, PayloadStream, StreamRequest, StreamResponse, UnaryRequest, UnaryResponse,
};
use connectrpc::{ConnectError, Interceptor, Protocol, RequestContext, async_trait};

const TIMEOUT_HEADER: &str = "connect-timeout-ms";
const MAX_DIGITS: usize = 10;

pub struct ConnectTimeout;

#[async_trait]
impl Interceptor for ConnectTimeout {
    async fn intercept_unary(&self, req: UnaryRequest, next: Next<'_>) -> Result<UnaryResponse, ConnectError> {
        unexpired(&req.ctx)?;
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
    match asserted_millis(ctx)? {
        Some(millis) if millis <= 0 => Err(ConnectError::deadline_exceeded("context deadline exceeded")),
        _ => Ok(()),
    }
}

fn asserted_millis(ctx: &RequestContext) -> Result<Option<i64>, ConnectError> {
    if ctx.protocol().is_some_and(|protocol| protocol != Protocol::Connect) {
        return Ok(None);
    }
    let Some(value) = ctx.headers().get(TIMEOUT_HEADER) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(value.as_bytes());
    match text.len() {
        0 => Ok(None),
        length if length > MAX_DIGITS => Err(ConnectError::invalid_argument(format!(
            "parse timeout: {} has >10 digits",
            go_quote(&text)
        ))),
        _ => text.parse::<i64>().map(Some).map_err(|_| {
            ConnectError::invalid_argument(format!(
                "parse timeout: strconv.ParseInt: parsing {}: invalid syntax",
                go_quote(&text)
            ))
        }),
    }
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

    fn millis_for(value: &str) -> Result<Option<i64>, String> {
        let mut headers = http::HeaderMap::new();
        headers.insert(TIMEOUT_HEADER, value.parse().unwrap());
        asserted_millis(&RequestContext::new(headers).with_protocol(Some(Protocol::Connect)))
            .map_err(|e| e.message.unwrap_or_default())
    }

    #[test]
    fn timeouts_parse_like_connect_go() {
        assert_eq!(millis_for(""), Ok(None));
        assert_eq!(millis_for("1500"), Ok(Some(1500)));
        assert_eq!(millis_for("-1"), Ok(Some(-1)));
        assert_eq!(millis_for("+7"), Ok(Some(7)));
        assert_eq!(
            millis_for("abc"),
            Err("parse timeout: strconv.ParseInt: parsing \"abc\": invalid syntax".to_owned())
        );
        assert_eq!(
            millis_for("12345678901"),
            Err("parse timeout: \"12345678901\" has >10 digits".to_owned())
        );
    }

    #[test]
    fn grpc_requests_are_left_alone() {
        let mut headers = http::HeaderMap::new();
        headers.insert(TIMEOUT_HEADER, "abc".parse().unwrap());
        let ctx = RequestContext::new(headers).with_protocol(Some(Protocol::Grpc));
        assert!(matches!(asserted_millis(&ctx), Ok(None)));
    }
}
