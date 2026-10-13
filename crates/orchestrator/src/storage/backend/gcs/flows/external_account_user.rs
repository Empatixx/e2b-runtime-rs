use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::super::{credsfile, json};
use super::{FlowToken, TokenFlow, default_client};
use crate::storage::backend::gcs::external_account::refresh;

const INVALID: &str = "credentials: invalid external_account_authorized_user configuration";

#[derive(Deserialize, Default)]
#[serde(default)]
struct UserFile {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_url: String,
    universe_domain: String,
}

pub struct ExternalAccountUserFlow {
    client_id: String,
    client_secret: String,
    token_url: String,
    universe_domain: String,
    refresh_token: Mutex<String>,
    client: Client,
}

impl ExternalAccountUserFlow {
    pub fn from_json(json: &[u8]) -> Result<Arc<Self>, String> {
        let file: UserFile = json::unmarshal(json, credsfile::EXTERNAL_ACCOUNT_AUTHORIZED_USER)?;
        let complete = [
            &file.client_id,
            &file.client_secret,
            &file.refresh_token,
            &file.token_url,
        ]
        .iter()
        .all(|field| !field.is_empty());
        match complete {
            false => Err(INVALID.to_owned()),
            true => Ok(Arc::new(Self {
                client_id: file.client_id,
                client_secret: file.client_secret,
                token_url: file.token_url,
                universe_domain: file.universe_domain,
                refresh_token: Mutex::new(file.refresh_token),
                client: default_client()?,
            })),
        }
    }
}

#[async_trait::async_trait]
impl TokenFlow for ExternalAccountUserFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        let mut refresh_token = self.refresh_token.lock().await;
        let refreshed = refresh(
            &self.client,
            &self.token_url,
            &refresh_token,
            &self.client_id,
            &self.client_secret,
        )
        .await?;
        if !refreshed.refresh_token.is_empty() {
            *refresh_token = refreshed.refresh_token;
        }
        Ok(FlowToken {
            value: refreshed.access_token,
            expiry: Some(refreshed.expiry),
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

    fn json(url: &str) -> String {
        format!(
            r#"{{"type":"external_account_authorized_user","client_id":"id","client_secret":"secret","refresh_token":"rt","token_url":"{url}"}}"#
        )
    }

    #[test]
    fn incomplete_files_are_rejected() {
        assert_eq!(ExternalAccountUserFlow::from_json(b"{}").err().unwrap(), INVALID);
        assert_eq!(
            ExternalAccountUserFlow::from_json(br#"{"client_id":"a","client_secret":"b","refresh_token":"c"}"#)
                .err()
                .unwrap(),
            INVALID
        );
    }

    #[tokio::test]
    async fn the_refresh_grant_uses_basic_auth_and_rotates_the_refresh_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_request(&mut connection).await;
            let body = r#"{"access_token":"at","expires_in":3600,"refresh_token":"rt2"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        let flow =
            ExternalAccountUserFlow::from_json(json(&format!("http://{address}/v1/oauthtoken")).as_bytes()).unwrap();

        let token = flow.token().await.unwrap();
        let request = server.await.unwrap();

        assert_eq!(token.value, "at");
        assert!(token.expiry.is_some());
        assert!(request.starts_with("POST /v1/oauthtoken HTTP/1.1"));
        assert!(request.contains("authorization: Basic aWQ6c2VjcmV0"));
        assert!(request.ends_with("grant_type=refresh_token&refresh_token=rt"));
        assert_eq!(*flow.refresh_token.lock().await, "rt2");
    }
}
