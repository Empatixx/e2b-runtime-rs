use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;

use super::super::{credsfile, json};
use super::{CLOUD_PLATFORM_SCOPE, FlowProfile, FlowToken, TokenFlow, build, default_client};
use crate::storage::backend::gcs::external_account::impersonate;

const MISSING: &str = "missing 'source_credentials' field or 'service_account_impersonation_url' in credentials";

#[derive(Deserialize, Default)]
#[serde(default)]
struct ImpersonatedFile {
    service_account_impersonation_url: String,
    delegates: Vec<String>,
    scopes: Vec<String>,
    source_credentials: Option<serde_json::Value>,
    universe_domain: String,
}

pub struct ImpersonatedFlow {
    url: String,
    delegates: Vec<String>,
    scopes: Vec<String>,
    universe_domain: String,
    source: Arc<dyn TokenFlow>,
    client: Client,
}

impl ImpersonatedFlow {
    pub fn from_json(json: &[u8], profile: &FlowProfile) -> Result<Arc<Self>, String> {
        let file: ImpersonatedFile = json::unmarshal(json, credsfile::IMPERSONATED)?;
        let source_json = match (
            &file.source_credentials,
            file.service_account_impersonation_url.is_empty(),
        ) {
            (Some(source), false) => source.to_string(),
            _ => return Err(MISSING.to_owned()),
        };
        let source = build(source_json.as_bytes(), &profile.with_scopes(&[CLOUD_PLATFORM_SCOPE]))?;
        let scopes = match profile.scopes.is_empty() {
            true => file.scopes,
            false => profile.scopes.clone(),
        };
        Ok(Arc::new(Self {
            url: file.service_account_impersonation_url,
            delegates: file.delegates,
            scopes,
            universe_domain: file.universe_domain,
            source,
            client: default_client()?,
        }))
    }

    #[cfg(test)]
    fn with_source(url: &str, delegates: &[&str], scopes: &[&str], source: Arc<dyn TokenFlow>) -> Self {
        Self {
            url: url.to_owned(),
            delegates: delegates.iter().map(|d| (*d).to_owned()).collect(),
            scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
            universe_domain: String::new(),
            source,
            client: Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl TokenFlow for ImpersonatedFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        let source = self.source.token().await?;
        impersonate(&self.client, &self.url, &source.value, &self.scopes, &self.delegates, 0)
            .await
            .map(|(value, expiry)| FlowToken {
                value,
                expiry: Some(expiry),
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

    fn profile(scopes: &[&str]) -> FlowProfile {
        FlowProfile::new(scopes, false)
    }

    struct Fixed;

    #[async_trait::async_trait]
    impl TokenFlow for Fixed {
        async fn token(&self) -> Result<FlowToken, String> {
            Ok(FlowToken {
                value: "source-token".to_owned(),
                expiry: None,
            })
        }
    }

    #[test]
    fn the_file_needs_a_source_and_an_impersonation_url() {
        assert_eq!(
            ImpersonatedFlow::from_json(b"{}", &profile(&[])).err().unwrap(),
            MISSING
        );
        assert_eq!(
            ImpersonatedFlow::from_json(br#"{"service_account_impersonation_url":"u"}"#, &profile(&[]))
                .err()
                .unwrap(),
            MISSING
        );
        assert_eq!(
            ImpersonatedFlow::from_json(
                br#"{"service_account_impersonation_url":"u","source_credentials":{"type":"nope"}}"#,
                &profile(&[])
            )
            .err()
            .unwrap(),
            "credentials: unsupported filetype \"nope\""
        );
    }

    #[test]
    fn user_scopes_win_over_the_file_scopes() {
        let json = br#"{"service_account_impersonation_url":"u","scopes":["file"],"source_credentials":{"type":"authorized_user","client_id":"a","client_secret":"b","refresh_token":"c"}}"#;

        assert_eq!(
            ImpersonatedFlow::from_json(json, &profile(&[])).unwrap().scopes,
            ["file"]
        );
        assert_eq!(
            ImpersonatedFlow::from_json(json, &profile(&["user"])).unwrap().scopes,
            ["user"]
        );
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

        let token = flow.token().await.unwrap();
        let request = server.await.unwrap();

        assert_eq!(token.value, "sa-token");
        assert!(token.expiry.is_some());
        assert!(request.contains("authorization: Bearer source-token"));
        assert!(request.ends_with(r#"{"delegates":["d@x"],"lifetime":"3600s","scope":["s1"]}"#));
    }

    #[test]
    fn the_source_credentials_get_the_cloud_platform_scope_and_inherit_the_self_signed_choice() {
        let json = serde_json::json!({
            "service_account_impersonation_url": "u",
            "source_credentials": {"type": "service_account", "client_email": "a@b", "private_key": "nope"}
        });

        let error = ImpersonatedFlow::from_json(json.to_string().as_bytes(), &FlowProfile::new(&[], true))
            .err()
            .unwrap();

        assert!(error.starts_with("credentials: could not parse key: "), "{error}");
    }

    #[test]
    fn the_universe_domain_comes_from_the_impersonation_file() {
        let json = br#"{"service_account_impersonation_url":"u","universe_domain":"example.com","source_credentials":{"type":"authorized_user","client_id":"a","client_secret":"b","refresh_token":"c"}}"#;

        assert_eq!(
            ImpersonatedFlow::from_json(json, &profile(&[]))
                .unwrap()
                .universe_domain(),
            "example.com"
        );
    }
}
