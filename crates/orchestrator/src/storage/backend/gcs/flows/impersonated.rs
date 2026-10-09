use std::sync::Arc;
use std::time::SystemTime;

use reqwest::Client;
use serde::Deserialize;

use super::{CLOUD_PLATFORM_SCOPE, TokenFlow, build};
use crate::storage::backend::gcs::external_account::impersonate;

const MISSING: &str = "missing 'source_credentials' field or 'service_account_impersonation_url' in credentials";

#[derive(Deserialize, Default)]
#[serde(default)]
struct ImpersonatedFile {
    service_account_impersonation_url: String,
    delegates: Vec<String>,
    scopes: Vec<String>,
    source_credentials: Option<serde_json::Value>,
}

pub struct ImpersonatedFlow {
    url: String,
    delegates: Vec<String>,
    scopes: Vec<String>,
    source: Arc<dyn TokenFlow>,
    client: Client,
}

impl ImpersonatedFlow {
    pub fn from_json(json: &[u8], scopes: &[&str]) -> Result<Arc<Self>, String> {
        let file: ImpersonatedFile = serde_json::from_slice(json).map_err(|e| e.to_string())?;
        let source_json = match (
            &file.source_credentials,
            file.service_account_impersonation_url.is_empty(),
        ) {
            (Some(source), false) => source.to_string(),
            _ => return Err(MISSING.to_owned()),
        };
        let source = build(source_json.as_bytes(), &[CLOUD_PLATFORM_SCOPE])?;
        let scopes = match scopes.is_empty() {
            true => file.scopes,
            false => scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        };
        Ok(Arc::new(Self {
            url: file.service_account_impersonation_url,
            delegates: file.delegates,
            scopes,
            source,
            client: Client::new(),
        }))
    }

    #[cfg(test)]
    fn with_source(url: &str, delegates: &[&str], scopes: &[&str], source: Arc<dyn TokenFlow>) -> Self {
        Self {
            url: url.to_owned(),
            delegates: delegates.iter().map(|d| (*d).to_owned()).collect(),
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
            source,
            client: Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl TokenFlow for ImpersonatedFlow {
    async fn token(&self) -> Result<(String, Option<SystemTime>), String> {
        let (source_token, _) = self.source.token().await?;
        impersonate(&self.client, &self.url, &source_token, &self.scopes, &self.delegates, 0)
            .await
            .map(|(token, expiry)| (token, Some(expiry)))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::storage::backend::azure::testing::read_request;

    struct Fixed;

    #[async_trait::async_trait]
    impl TokenFlow for Fixed {
        async fn token(&self) -> Result<(String, Option<SystemTime>), String> {
            Ok(("source-token".to_owned(), None))
        }
    }

    #[test]
    fn the_file_needs_a_source_and_an_impersonation_url() {
        assert_eq!(ImpersonatedFlow::from_json(b"{}", &[]).err().unwrap(), MISSING);
        assert_eq!(
            ImpersonatedFlow::from_json(br#"{"service_account_impersonation_url":"u"}"#, &[])
                .err()
                .unwrap(),
            MISSING
        );
        assert_eq!(
            ImpersonatedFlow::from_json(
                br#"{"service_account_impersonation_url":"u","source_credentials":{"type":"nope"}}"#,
                &[]
            )
            .err()
            .unwrap(),
            "credentials: unsupported filetype \"nope\""
        );
    }

    #[test]
    fn user_scopes_win_over_the_file_scopes() {
        let json = br#"{"service_account_impersonation_url":"u","scopes":["file"],"source_credentials":{"type":"authorized_user","client_id":"a","client_secret":"b","refresh_token":"c"}}"#;

        assert_eq!(ImpersonatedFlow::from_json(json, &[]).unwrap().scopes, ["file"]);
        assert_eq!(ImpersonatedFlow::from_json(json, &["user"]).unwrap().scopes, ["user"]);
    }

    #[tokio::test]
    async fn the_source_token_generates_the_service_account_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_request(&mut connection).await;
            let body = r#"{"accessToken":"sa-token","expireTime":"2099-01-01T00:00:00Z"}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        let flow = ImpersonatedFlow::with_source(&format!("http://{address}/gen"), &["d@x"], &["s1"], Arc::new(Fixed));

        let (token, expiry) = flow.token().await.unwrap();
        let request = server.await.unwrap();

        assert_eq!(token, "sa-token");
        assert!(expiry.is_some());
        assert!(request.contains("authorization: Bearer source-token"));
        assert!(request.ends_with(r#"{"delegates":["d@x"],"lifetime":"3600s","scope":["s1"]}"#));
    }
}
