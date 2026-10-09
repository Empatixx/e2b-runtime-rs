use std::time::Duration;

use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, Response};

const STANDARD_STATUSES: [u16; 6] = [408, 429, 500, 502, 503, 504];
const IMDS_STATUSES: [u16; 14] = [404, 410, 429, 500, 501, 502, 503, 504, 505, 506, 507, 508, 510, 511];

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub retry_delay: Duration,
    pub max_retry_delay: Duration,
    pub statuses: &'static [u16],
    pub try_timeout: Option<Duration>,
}

impl RetryPolicy {
    pub fn standard() -> Self {
        Self {
            max_retries: 3,
            retry_delay: Duration::from_millis(800),
            max_retry_delay: Duration::from_secs(60),
            statuses: &STANDARD_STATUSES,
            try_timeout: None,
        }
    }

    pub fn imds() -> Self {
        Self {
            max_retries: 6,
            retry_delay: Duration::from_secs(2),
            max_retry_delay: Duration::from_secs(25),
            statuses: &IMDS_STATUSES,
            try_timeout: Some(Duration::from_secs(60)),
        }
    }

    #[cfg(test)]
    pub fn without_delay(self) -> Self {
        Self {
            retry_delay: Duration::ZERO,
            ..self
        }
    }

    pub fn delay(&self, attempt: u32, jitter: f64) -> Duration {
        let factor = 1u32.checked_shl(attempt).map_or(u32::MAX, |shifted| shifted - 1);
        self.retry_delay
            .saturating_mul(factor)
            .mul_f64(jitter)
            .min(self.max_retry_delay)
    }

    fn retries(&self, outcome: &Result<Response, reqwest::Error>) -> bool {
        match outcome {
            Ok(response) => self.statuses.contains(&response.status().as_u16()),
            Err(_) => true,
        }
    }

    fn wait_for(&self, attempt: u32, outcome: &Result<Response, reqwest::Error>) -> Option<Duration> {
        let advised = outcome
            .as_ref()
            .ok()
            .and_then(|response| retry_after(response.headers()));
        match advised {
            Some(advised) if advised > self.max_retry_delay => None,
            Some(advised) => Some(advised),
            None => Some(self.delay(attempt, rand::random::<f64>() / 2.0 + 0.8)),
        }
    }
}

pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    ["retry-after-ms", "x-ms-retry-after-ms"]
        .iter()
        .find_map(|name| number(headers, name).map(Duration::from_millis))
        .or_else(|| number(headers, "retry-after").map(Duration::from_secs))
        .or_else(|| http_date_delay(headers))
}

fn number(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn http_date_delay(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    let until = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (until.to_utc() - chrono::Utc::now())
        .to_std()
        .ok()
        .filter(|delay| !delay.is_zero())
}

pub async fn send(policy: &RetryPolicy, request: RequestBuilder) -> Result<Response, reqwest::Error> {
    let mut attempt = 1;
    loop {
        let outcome = match request.try_clone() {
            Some(clone) => attempted(policy, clone).await,
            None => return attempted(policy, request).await,
        };
        let wait = match (
            attempt <= policy.max_retries && policy.retries(&outcome),
            policy.wait_for(attempt, &outcome),
        ) {
            (true, Some(wait)) => wait,
            _ => return outcome,
        };
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

async fn attempted(policy: &RetryPolicy, request: RequestBuilder) -> Result<Response, reqwest::Error> {
    match policy.try_timeout {
        Some(timeout) => request.timeout(timeout).send().await,
        None => request.send().await,
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::super::testing::serve;
    use super::*;

    #[test]
    fn delays_follow_the_exponential_formula_with_jitter_and_a_cap() {
        let policy = RetryPolicy::standard();

        assert_eq!(policy.delay(1, 1.0), Duration::from_millis(800));
        assert_eq!(policy.delay(2, 1.0), Duration::from_millis(2400));
        assert_eq!(policy.delay(3, 1.0), Duration::from_millis(5600));
        assert_eq!(policy.delay(3, 1.3), Duration::from_millis(7280));
        assert_eq!(policy.delay(10, 1.0), Duration::from_secs(60));
        assert_eq!(RetryPolicy::imds().delay(4, 1.0), Duration::from_secs(25));
        assert_eq!(policy.delay(70, 1.0), Duration::from_secs(60));
    }

    #[test]
    fn the_imds_policy_retries_six_times_on_go_status_list() {
        let policy = RetryPolicy::imds();

        assert_eq!(policy.max_retries, 6);
        assert_eq!(policy.retry_delay, Duration::from_secs(2));
        assert_eq!(policy.try_timeout, Some(Duration::from_secs(60)));
        assert!(policy.statuses.contains(&404) && policy.statuses.contains(&410) && policy.statuses.contains(&511));
        assert!(!policy.statuses.contains(&408) && !policy.statuses.contains(&509));
    }

    #[test]
    fn retry_after_headers_are_read_in_go_order() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(7)));
        headers.insert("x-ms-retry-after-ms", HeaderValue::from_static("250"));
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(250)));
        headers.insert("retry-after-ms", HeaderValue::from_static("100"));
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(100)));
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn only_listed_statuses_are_retried_and_the_last_response_is_returned() {
        let policy = RetryPolicy::imds().without_delay();
        let replies = (0..7).map(|_| (404, "text/plain", String::new())).collect();
        let (base, handle) = serve(replies).await;

        let response = send(&policy, reqwest::Client::new().get(&base)).await.unwrap();

        assert_eq!(response.status().as_u16(), 404);
        assert_eq!(handle.await.unwrap().len(), 7);
    }

    #[tokio::test]
    async fn unlisted_statuses_are_returned_immediately() {
        let policy = RetryPolicy::imds().without_delay();
        let (base, handle) = serve(vec![(400, "text/plain", String::new())]).await;

        let response = send(&policy, reqwest::Client::new().get(&base)).await.unwrap();

        assert_eq!(response.status().as_u16(), 400);
        assert_eq!(handle.await.unwrap().len(), 1);
    }
}
