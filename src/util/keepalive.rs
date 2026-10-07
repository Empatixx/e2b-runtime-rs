use std::time::Duration;

use axum::http::HeaderMap;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(90);

pub fn keepalive_interval(headers: &HeaderMap) -> Duration {
    headers
        .get("keepalive-ping-interval")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_INTERVAL, Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn interval(value: &str) -> Duration {
        let mut headers = HeaderMap::new();
        headers.insert("keepalive-ping-interval", HeaderValue::from_str(value).unwrap());
        keepalive_interval(&headers)
    }

    #[test]
    fn header_or_default() {
        assert_eq!(keepalive_interval(&HeaderMap::new()), Duration::from_secs(90));
        assert_eq!(interval("5"), Duration::from_secs(5));
        assert_eq!(interval("+5"), Duration::from_secs(5));
        assert_eq!(interval("abc"), Duration::from_secs(90));
        assert_eq!(interval("0"), Duration::from_secs(90));
        assert_eq!(interval("-1"), Duration::from_secs(90));
    }
}
