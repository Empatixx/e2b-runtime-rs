use std::time::Duration;

use reqwest::Client;

const TIMEOUT: Duration = Duration::from_secs(30);

pub fn default_client() -> Result<Client, String> {
    Client::builder().timeout(TIMEOUT).build().map_err(|e| e.to_string())
}

pub fn request_error(method: &str, url: &str, error: &reqwest::Error) -> String {
    format!("{method} {}: {}", crate::go_strconv::quote(url), innermost(error))
}

fn innermost(error: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_errors_read_like_go_url_errors() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/x")
            .send()
            .await
            .unwrap_err();

        let text = request_error("Get", "http://127.0.0.1:1/x", &error);

        assert!(text.starts_with("Get \"http://127.0.0.1:1/x\": "), "{text}");
    }
}
