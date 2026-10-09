use reqwest::Client;

use super::aad::Authority;
use super::token_source::{BearerToken, SourceError, TokenSource};

const CREDENTIAL: &str = "UsernamePasswordCredential";

pub struct UsernamePasswordCredential {
    pub client_id: String,
    pub username: String,
    pub password: String,
    pub authority: Authority,
    pub client: Client,
}

#[async_trait::async_trait]
impl TokenSource for UsernamePasswordCredential {
    async fn token(&self, scope: &str) -> Result<BearerToken, SourceError> {
        self.authority
            .request_token(
                &self.client,
                CREDENTIAL,
                "",
                scope,
                vec![
                    ("grant_type", "password".to_owned()),
                    ("username", self.username.clone()),
                    ("password", self.password.clone()),
                    ("client_id", self.client_id.clone()),
                    ("client_info", "1".to_owned()),
                ],
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn the_password_grant_is_posted_with_the_user() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = super::super::testing::read_request(&mut connection).await;
            let body = r#"{"error":"invalid_grant"}"#;
            let reply = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        let credential = UsernamePasswordCredential {
            client_id: "client".to_owned(),
            username: "me@example.com".to_owned(),
            password: "pw".to_owned(),
            authority: Authority::with_token_endpoint(format!("http://{address}/t/oauth2/v2.0/token")),
            client: Client::new(),
        };

        let error = credential.token("s").await.unwrap_err();
        let request = server.await.unwrap();

        assert!(request.contains("grant_type=password"));
        assert!(request.contains("username=me%40example.com"));
        assert!(
            matches!(&error, SourceError::Failed(text) if text.starts_with("UsernamePasswordCredential authentication failed. \nPOST http://") && text.contains("RESPONSE 400: 400 Bad Request"))
        );
    }
}
