use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::super::flows::read_three_legged;
use super::super::flows::{Dialect, FlowToken, TokenFlow, default_client, request_error};
use super::super::form::UrlEncodedForm;

const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const NO_REFRESH_TOKEN: &str = "oauth2: token expired and refresh token is not set";

#[derive(Deserialize, Default)]
#[serde(default)]
struct UserFile {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_uri: String,
}

pub struct UserFlow {
    client_id: String,
    client_secret: String,
    token_url: String,
    refresh_token: Mutex<String>,
    client: Client,
}

impl UserFlow {
    pub fn from_json(json: &[u8]) -> Result<Arc<Self>, String> {
        let file: UserFile = serde_json::from_slice(json).map_err(|e| e.to_string())?;
        let token_url = match file.token_uri.is_empty() {
            true => DEFAULT_TOKEN_URL.to_owned(),
            false => file.token_uri,
        };
        Ok(Arc::new(Self {
            client_id: file.client_id,
            client_secret: file.client_secret,
            token_url,
            refresh_token: Mutex::new(file.refresh_token),
            client: default_client()?,
        }))
    }

    fn form<'a>(&'a self, refresh_token: &'a str) -> Vec<(&'a str, &'a str)> {
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if !self.client_secret.is_empty() {
            form.push(("client_secret", self.client_secret.as_str()));
        }
        form
    }
}

#[async_trait::async_trait]
impl TokenFlow for UserFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        let mut refresh_token = self.refresh_token.lock().await;
        if refresh_token.is_empty() {
            return Err(NO_REFRESH_TOKEN.to_owned());
        }
        let response = self
            .client
            .post(&self.token_url)
            .url_encoded_form(self.form(&refresh_token))
            .send()
            .await
            .map_err(|e| request_error("Post", &self.token_url, &e))?;
        let token = read_three_legged(response, Dialect::Oauth2).await?;
        if !token.refresh_token.is_empty() {
            *refresh_token = token.refresh_token;
        }
        Ok(FlowToken {
            value: token.access_token,
            expiry: token.expiry,
        })
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

    fn file(url: &str, refresh_token: &str) -> String {
        format!(
            r#"{{"type":"authorized_user","client_id":"id","client_secret":"secret","refresh_token":"{refresh_token}","token_uri":"{url}"}}"#
        )
    }

    #[tokio::test]
    async fn the_refresh_grant_goes_to_the_token_uri_of_the_file_with_the_client_in_params() {
        let (url, server) = serve(
            200,
            "application/json",
            r#"{"access_token":"at","expires_in":3600,"refresh_token":"rt2"}"#,
        )
        .await;
        let flow = UserFlow::from_json(file(&url, "rt").as_bytes()).unwrap();

        let token = flow.token().await.unwrap();
        let request = server.await.unwrap();

        assert_eq!(token.value, "at");
        assert!(request.ends_with("client_id=id&client_secret=secret&grant_type=refresh_token&refresh_token=rt"));
        assert_eq!(*flow.refresh_token.lock().await, "rt2");
    }

    #[tokio::test]
    async fn a_missing_refresh_token_fails_before_any_request() {
        let flow = UserFlow::from_json(file("http://127.0.0.1:1/token", "").as_bytes()).unwrap();

        assert_eq!(flow.token().await.unwrap_err(), NO_REFRESH_TOKEN);
    }

    #[tokio::test]
    async fn an_empty_client_secret_is_not_sent() {
        let (url, server) = serve(200, "application/json", r#"{"access_token":"at"}"#).await;
        let json = format!(r#"{{"client_id":"id","refresh_token":"rt","token_uri":"{url}"}}"#);

        UserFlow::from_json(json.as_bytes()).unwrap().token().await.unwrap();

        assert!(
            server
                .await
                .unwrap()
                .ends_with("client_id=id&grant_type=refresh_token&refresh_token=rt")
        );
    }

    #[tokio::test]
    async fn oauth_errors_use_the_oauth2_prefix() {
        let (url, _server) = serve(
            400,
            "application/json",
            r#"{"error":"invalid_grant","error_description":"Token has been expired"}"#,
        )
        .await;
        let flow = UserFlow::from_json(file(&url, "rt").as_bytes()).unwrap();

        assert_eq!(
            flow.token().await.unwrap_err(),
            r#"oauth2: "invalid_grant" "Token has been expired""#
        );
    }
}
