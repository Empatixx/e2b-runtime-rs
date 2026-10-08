use std::time::Duration;

use reqwest::{RequestBuilder, Response};

const ATTEMPTS: u32 = 3;
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const FACTOR: u32 = 3;
const JITTER: f64 = 0.1;
const RETRY_STATUS_CODES: [u16; 8] = [408, 429, 500, 502, 503, 504, 499, 522];

pub async fn send(request: RequestBuilder) -> Result<Response, reqwest::Error> {
    let mut backoff = FIRST_BACKOFF;
    let mut attempt = 1;
    loop {
        let sent = request
            .try_clone()
            .expect("registry requests have no streaming body")
            .send()
            .await;
        let retry = match &sent {
            Ok(response) => RETRY_STATUS_CODES.contains(&response.status().as_u16()),
            Err(e) => e.is_timeout() || e.is_connect() || e.is_body() || e.is_request(),
        };
        if !retry || attempt == ATTEMPTS {
            return sent;
        }
        tracing::warn!(attempt, "retrying registry request");
        tokio::time::sleep(jittered(backoff)).await;
        backoff *= FACTOR;
        attempt += 1;
    }
}

fn jittered(backoff: Duration) -> Duration {
    backoff.mul_f64(1.0 + rand::random::<f64>() * JITTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_adds_at_most_ten_percent() {
        let delay = jittered(Duration::from_secs(1));

        assert!(delay >= Duration::from_secs(1) && delay <= Duration::from_millis(1100));
    }
}
