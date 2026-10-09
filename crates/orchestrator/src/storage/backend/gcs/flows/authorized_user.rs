use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::super::form::UrlEncodedForm;
use super::super::{credsfile, go_json};
use super::response::{Dialect, read_three_legged};
use super::{FlowToken, TokenFlow, default_client, request_error};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Deserialize, Default)]
#[serde(default)]
struct UserFile {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    universe_domain: String,
}

pub struct AuthorizedUserFlow {
    client_id: String,
    client_secret: String,
    refresh_token: Mutex<String>,
    token_url: String,
    universe_domain: String,
    client: Client,
}

impl AuthorizedUserFlow {
    pub fn from_json(json: &[u8]) -> Result<Arc<Self>, String> {
        Self::with_token_url(json, TOKEN_URL)
    }

    fn with_token_url(json: &[u8], token_url: &str) -> Result<Arc<Self>, String> {
        let file: UserFile = go_json::unmarshal(json, credsfile::USER_CREDENTIALS)?;
        if file.client_id.is_empty() {
            return Err("auth: client ID must be provided".to_owned());
        }
        if file.client_secret.is_empty() {
            return Err("auth: client secret must be provided".to_owned());
        }
        if file.refresh_token.is_empty() {
            return Err("auth: refresh token must be provided".to_owned());
        }
        Ok(Arc::new(Self {
            client_id: file.client_id,
            client_secret: file.client_secret,
            refresh_token: Mutex::new(file.refresh_token),
            token_url: token_url.to_owned(),
            universe_domain: file.universe_domain,
            client: default_client()?,
        }))
    }
}

#[async_trait::async_trait]
impl TokenFlow for AuthorizedUserFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        let mut refresh_token = self.refresh_token.lock().await;
        let response = self
            .client
            .post(&self.token_url)
            .url_encoded_form([
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .map_err(|e| request_error("Post", &self.token_url, &e))?;
        let token = read_three_legged(response, Dialect::Auth).await?;
        if !token.refresh_token.is_empty() {
            *refresh_token = token.refresh_token;
        }
        Ok(FlowToken {
            value: token.access_token,
            expiry: token.expiry,
        })
    }

    fn universe_domain(&self) -> String {
        self.universe_domain.clone()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::storage::backend::azure::testing::read_request;

    async fn serve(
        status: u16,
        content_type: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_request(&mut connection).await;
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        (format!("http://{address}/token"), handle)
    }

    const FILE: &[u8] = br#"{"type":"authorized_user","client_id":"id","client_secret":"secret","refresh_token":"rt"}"#;

    #[test]
    fn the_file_must_carry_client_and_refresh_token() {
        assert_eq!(
            AuthorizedUserFlow::from_json(b"{}").err().unwrap(),
            "auth: client ID must be provided"
        );
        assert_eq!(
            AuthorizedUserFlow::from_json(br#"{"client_id":"a"}"#).err().unwrap(),
            "auth: client secret must be provided"
        );
        assert_eq!(
            AuthorizedUserFlow::from_json(br#"{"client_id":"a","client_secret":"b"}"#)
                .err()
                .unwrap(),
            "auth: refresh token must be provided"
        );
    }

    #[tokio::test]
    async fn the_refresh_grant_is_posted_with_the_client_in_the_params() {
        let (url, handle) = serve(
            200,
            "application/json",
            r#"{"access_token":"at","expires_in":3600,"refresh_token":"rt2"}"#,
        )
        .await;
        let flow = AuthorizedUserFlow::with_token_url(FILE, &url).unwrap();

        let token = flow.token().await.unwrap();
        let request = handle.await.unwrap();

        assert_eq!(token.value, "at");
        assert!(token.expiry.is_some());
        assert!(request.ends_with("client_id=id&client_secret=secret&grant_type=refresh_token&refresh_token=rt"));
        assert_eq!(*flow.refresh_token.lock().await, "rt2");
    }

    #[tokio::test]
    async fn oauth_errors_read_like_the_sdk() {
        let (url, _handle) = serve(
            400,
            "application/json",
            r#"{"error":"invalid_grant","error_description":"Token has been expired"}"#,
        )
        .await;
        let flow = AuthorizedUserFlow::with_token_url(FILE, &url).unwrap();

        assert_eq!(
            flow.token().await.unwrap_err(),
            r#"auth: "invalid_grant" "Token has been expired""#
        );
    }

    #[tokio::test]
    async fn unparseable_failures_show_the_status_and_body() {
        let (url, _handle) = serve(502, "text/html", "<html>bad gateway</html>").await;
        let flow = AuthorizedUserFlow::with_token_url(FILE, &url).unwrap();

        assert_eq!(
            flow.token().await.unwrap_err(),
            "auth: cannot fetch token: 502\nResponse: <html>bad gateway</html>"
        );
    }
}
