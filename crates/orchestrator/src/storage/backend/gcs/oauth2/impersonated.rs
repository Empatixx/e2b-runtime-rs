use std::sync::Arc;
use std::time::SystemTime;

use chrono::DateTime;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::super::flows::{FlowToken, TokenFlow, default_client};
use super::super::{credsfile, go_json};

const DEFAULT_LIFETIME: &str = "3600s";

#[derive(Serialize)]
struct GenerateAccessToken<'a> {
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    delegates: &'a [String],
    lifetime: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    scope: &'a [String],
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct GeneratedToken {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expireTime")]
    expire_time: String,
}

pub struct ImpersonatedFlow {
    url: String,
    scopes: Vec<String>,
    delegates: Vec<String>,
    source: Arc<dyn TokenFlow>,
    client: Client,
}

impl ImpersonatedFlow {
    pub fn new(
        url: String,
        scopes: &[String],
        delegates: Vec<String>,
        source: Arc<dyn TokenFlow>,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            url,
            scopes: scopes.to_vec(),
            delegates,
            source,
            client: default_client()?,
        }))
    }

    async fn send(&self, source: &FlowToken) -> Result<(u16, String), String> {
        let body = serde_json::to_string(&GenerateAccessToken {
            delegates: &self.delegates,
            lifetime: DEFAULT_LIFETIME,
            scope: &self.scopes,
        })
        .map_err(|e| format!("oauth2/google: unable to marshal request: {e}"))?;
        let response = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .bearer_auth(&source.value)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "oauth2/google: unable to generate access token: {}",
                    super::super::flows::request_error("Post", &self.url, &e)
                )
            })?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|e| format!("oauth2/google: unable to read body: {e}"))?;
        Ok((status, text))
    }
}

#[async_trait::async_trait]
impl TokenFlow for ImpersonatedFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        let source = self.source.token().await.map_err(|e| {
            format!(
                "oauth2/google: unable to generate access token: Post {}: {e}",
                crate::go_strconv::quote(&self.url)
            )
        })?;
        let (status, text) = self.send(&source).await?;
        if !(200..=299).contains(&status) {
            return Err(format!("oauth2/google: status code {status}: {text}"));
        }
        let parsed: GeneratedToken = go_json::unmarshal(text.as_bytes(), credsfile::OAUTH2_IMPERSONATION_RESPONSE)
            .map_err(|e| format!("oauth2/google: unable to parse response: {e}"))?;
        let expiry = DateTime::parse_from_rfc3339(&parsed.expire_time)
            .map_err(|e| format!("oauth2/google: unable to parse expiry: {e}"))?;
        Ok(FlowToken {
            value: parsed.access_token,
            expiry: Some(SystemTime::from(expiry)),
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::storage::backend::azure::testing::read_request;

    struct Fixed(Result<&'static str, &'static str>);

    #[async_trait::async_trait]
    impl TokenFlow for Fixed {
        async fn token(&self) -> Result<FlowToken, String> {
            self.0
                .map(|value| FlowToken {
                    value: value.to_owned(),
                    expiry: None,
                })
                .map_err(str::to_owned)
        }
    }

    async fn serve(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_request(&mut connection).await;
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}/gen"), handle)
    }

    fn flow(url: String, source: Fixed) -> Arc<ImpersonatedFlow> {
        ImpersonatedFlow::new(url, &["s1".to_owned()], vec!["d@x".to_owned()], Arc::new(source)).unwrap()
    }

    #[tokio::test]
    async fn the_source_token_authorizes_the_generate_access_token_call() {
        let (url, server) = serve(200, r#"{"accessToken":"sa-token","expireTime":"2099-01-01T00:00:00Z"}"#).await;

        let token = flow(url, Fixed(Ok("source-token"))).token().await.unwrap();
        let request = server.await.unwrap();

        assert_eq!(token.value, "sa-token");
        assert!(token.expiry.is_some());
        assert!(request.contains("authorization: Bearer source-token"));
        assert!(request.ends_with(r#"{"delegates":["d@x"],"lifetime":"3600s","scope":["s1"]}"#));
    }

    #[tokio::test]
    async fn failures_use_the_oauth2_google_wording() {
        let (url, _server) = serve(403, "denied").await;

        assert_eq!(
            flow(url, Fixed(Ok("t"))).token().await.unwrap_err(),
            "oauth2/google: status code 403: denied"
        );
        assert_eq!(
            flow("http://127.0.0.1:1/gen".to_owned(), Fixed(Err("oauth2: boom")))
                .token()
                .await
                .unwrap_err(),
            "oauth2/google: unable to generate access token: Post \"http://127.0.0.1:1/gen\": oauth2: boom"
        );
    }
}
