use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};

pub fn rfc3339(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn rfc3339_nano(time: SystemTime) -> String {
    let full = DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let (seconds, fraction) = full.trim_end_matches('Z').split_once('.').unwrap_or((&full, ""));
    match fraction.trim_end_matches('0') {
        "" => format!("{seconds}Z"),
        digits => format!("{seconds}.{digits}Z"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(nanos: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_nanos(1_700_000_000_000_000_000 + nanos)
    }

    #[test]
    fn formats_like_go_time() {
        assert_eq!(rfc3339(at(500_000_000)), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_nano(at(500_000_000)), "2023-11-14T22:13:20.5Z");
        assert_eq!(rfc3339_nano(at(123_456_789)), "2023-11-14T22:13:20.123456789Z");
        assert_eq!(rfc3339_nano(at(0)), "2023-11-14T22:13:20Z");
    }
}
