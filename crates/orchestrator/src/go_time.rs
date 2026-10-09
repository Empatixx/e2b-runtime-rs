mod parse;

use std::time::{Duration, SystemTime};

pub use parse::parse_duration;

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

const MICRO: u128 = 1_000;
const MILLI: u128 = 1_000_000;
const SECOND: u128 = 1_000_000_000;

pub fn duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    match nanos {
        0 => "0s".to_owned(),
        _ if nanos < MICRO => format!("{nanos}ns"),
        _ if nanos < MILLI => format!("{}µs", decimal(nanos, 3)),
        _ if nanos < SECOND => format!("{}ms", decimal(nanos, 6)),
        _ => clock(nanos),
    }
}

fn clock(nanos: u128) -> String {
    let seconds = nanos / SECOND;
    let hours = (seconds >= 3600).then(|| format!("{}h", seconds / 3600));
    let minutes = (seconds >= 60).then(|| format!("{}m", seconds / 60 % 60));
    let rest = decimal(seconds % 60 * SECOND + nanos % SECOND, 9);
    format!("{}{}{rest}s", hours.unwrap_or_default(), minutes.unwrap_or_default())
}

fn decimal(value: u128, digits: u32) -> String {
    let unit = 10u128.pow(digits);
    let fraction = format!("{:0width$}", value % unit, width = digits as usize);
    match fraction.trim_end_matches('0') {
        "" => format!("{}", value / unit),
        trimmed => format!("{}.{trimmed}", value / unit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_like_go_duration_string() {
        let cases = [
            (Duration::ZERO, "0s"),
            (Duration::from_nanos(999), "999ns"),
            (Duration::from_nanos(1_100), "1.1µs"),
            (Duration::from_micros(1_500), "1.5ms"),
            (Duration::from_millis(900), "900ms"),
            (Duration::from_secs(65), "1m5s"),
            (Duration::from_secs(3600), "1h0m0s"),
            (Duration::from_nanos(600_003_118_234), "10m0.003118234s"),
            (Duration::from_millis(2_500), "2.5s"),
        ];
        for (input, expected) in cases {
            assert_eq!(duration(input), expected);
        }
    }

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
