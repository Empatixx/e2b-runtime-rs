use std::time::Duration;

use axum::http::HeaderMap;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(90);
const MAX_INTERVAL_SECONDS: i64 = i64::MAX / 1_000_000_000;

pub fn keepalive_interval(headers: &HeaderMap) -> Duration {
    headers
        .get("keepalive-ping-interval")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|seconds| (1..=MAX_INTERVAL_SECONDS).contains(seconds))
        .map_or(DEFAULT_INTERVAL, |seconds| Duration::from_secs(seconds as u64))
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

    #[test]
    fn invalid_intervals_fall_back_and_the_bounds_hold() {
        let default = Duration::from_secs(90);
        for (name, value, expected) in [
            ("invalid", "not-a-number", default),
            ("zero", "0", default),
            ("negative", "-1", default),
            ("overflow", "9223372037", default),
            ("one", "1", Duration::from_secs(1)),
            ("default", "90", default),
            ("maximum", "9223372036", Duration::from_secs(9_223_372_036)),
        ] {
            assert_eq!(interval(value), expected, "{name}");
        }
    }

    #[test]
    fn a_missing_header_reads_as_the_default() {
        assert_eq!(keepalive_interval(&HeaderMap::new()), DEFAULT_INTERVAL);
    }
}
