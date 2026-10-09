mod aws;
mod config;
mod executable;
mod exit_status;
mod sts;
mod subject;
mod x509;

use std::sync::Arc;
use std::time::SystemTime;

use reqwest::Client;

pub use sts::{Refreshed, impersonate, refresh};

use super::flows::{CLOUD_PLATFORM_SCOPE, FlowProfile, FlowToken, TokenFlow, default_client};
use config::ExternalAccountFile;
use subject::Subject;

pub struct ExternalAccount {
    file: ExternalAccountFile,
    subject: Subject,
    client: Client,
    scopes: Vec<String>,
}

impl std::fmt::Debug for ExternalAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalAccount")
            .field("audience", &self.file.audience)
            .finish_non_exhaustive()
    }
}

impl ExternalAccount {
    pub fn from_json(json: &[u8], profile: &FlowProfile) -> Result<Arc<Self>, String> {
        let file = ExternalAccountFile::parse(json)?;
        file.validate()?;
        let plain = default_client()?;
        let subject = Subject::from_file(&file, &plain)?;
        let client = match &subject {
            Subject::X509(x509) => x509.client()?,
            _ => plain,
        };
        Ok(Arc::new(Self {
            file,
            subject,
            client,
            scopes: profile.scopes.clone(),
        }))
    }

    async fn exchange(&self) -> Result<(String, SystemTime), String> {
        let subject_token = self.subject.token().await?;
        let impersonation_url = self.file.service_account_impersonation_url.as_str();
        let sts_scopes = match impersonation_url.is_empty() {
            true => self.scopes.clone(),
            false => vec![CLOUD_PLATFORM_SCOPE.to_owned()],
        };
        let (token, expiry) = sts::Exchange {
            client: &self.client,
            endpoint: &self.file.resolved_token_url(),
            audience: &self.file.audience,
            subject_token_type: &self.file.subject_token_type,
            subject_token: &subject_token,
            scopes: &sts_scopes,
            client_id: &self.file.client_id,
            client_secret: &self.file.client_secret,
            workforce_user_project: &self.file.workforce_pool_user_project,
            source: self.subject.provider_type(),
            impersonating: !impersonation_url.is_empty(),
            lifetime_configured: self.file.impersonation_lifetime_seconds() != 0,
        }
        .token()
        .await?;
        match impersonation_url.is_empty() {
            true => Ok((token, expiry)),
            false => {
                sts::impersonate(
                    &self.client,
                    impersonation_url,
                    &token,
                    &self.scopes,
                    &[],
                    self.file.impersonation_lifetime_seconds(),
                )
                .await
            }
        }
    }
}

#[async_trait::async_trait]
impl TokenFlow for ExternalAccount {
    async fn token(&self) -> Result<FlowToken, String> {
        self.exchange().await.map(|(value, expiry)| FlowToken {
            value,
            expiry: Some(expiry),
        })
    }

    fn universe_domain(&self) -> String {
        self.file.universe_domain.clone()
    }
}

#[cfg(test)]
mod tests {
    use object_store::CredentialProvider;
    use tokio::io::AsyncWriteExt;

    use super::super::flows::FlowCredentials;
    use super::*;
    use crate::storage::backend::azure::testing::read_request;

    fn profile(scopes: &[&str]) -> FlowProfile {
        FlowProfile::new(scopes, false)
    }

    async fn serve(replies: Vec<(u16, String)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut connection, _) = listener.accept().await.unwrap();
                seen.push(read_request(&mut connection).await);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                connection.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        (format!("http://{address}"), handle)
    }

    fn account_json(token_file: &str, sts: &str, extra: &str) -> String {
        format!(
            r#"{{"type":"external_account","audience":"//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/x","subject_token_type":"urn:ietf:params:oauth:token-type:jwt","token_url":"{sts}/v1/token","credential_source":{{"file":"{token_file}"}}{extra}}}"#
        )
    }

    #[tokio::test]
    async fn the_subject_token_is_exchanged_at_the_sts_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("subject");
        std::fs::write(&token_file, "jwt-subject\n").unwrap();
        let (sts, handle) = serve(vec![(
            200,
            r#"{"access_token":"gcp-token","expires_in":3600}"#.to_owned(),
        )])
        .await;
        let json = account_json(token_file.to_str().unwrap(), &sts, "");
        let credentials = FlowCredentials::from_json(json.as_bytes(), &profile(&["scope-a", "scope-b"])).unwrap();

        let credential = credentials.get_credential().await.unwrap();
        credentials.get_credential().await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(credential.bearer, "gcp-token");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /v1/token HTTP/1.1"));
        assert!(requests[0].contains("x-goog-api-client: gl-go/1.26.8 auth/unknown google-byoid-sdk source/file sa-impersonation/false config-lifetime/false"));
        assert!(requests[0].contains("subject_token=jwt-subject"));
        assert!(requests[0].contains("scope=scope-a+scope-b"));
        assert!(requests[0].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange"));
    }

    #[tokio::test]
    async fn impersonation_trades_the_sts_token_for_a_service_account_token() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("subject");
        std::fs::write(&token_file, "jwt").unwrap();
        let (server, handle) = serve(vec![
            (200, r#"{"access_token":"sts-token","expires_in":3600}"#.to_owned()),
            (
                200,
                r#"{"accessToken":"sa-token","expireTime":"2099-01-01T00:00:00Z"}"#.to_owned(),
            ),
        ])
        .await;
        let extra = format!(
            r#","service_account_impersonation_url":"{server}/impersonate","service_account_impersonation":{{"token_lifetime_seconds":600}}"#
        );
        let json = account_json(token_file.to_str().unwrap(), &server, &extra);
        let account = ExternalAccount::from_json(json.as_bytes(), &profile(&["scope-a"])).unwrap();

        let (token, _) = account.exchange().await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token, "sa-token");
        assert!(requests[0].contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform"));
        assert!(requests[0].contains("sa-impersonation/true config-lifetime/true"));
        assert!(requests[1].starts_with("POST /impersonate HTTP/1.1"));
        assert!(requests[1].contains("authorization: Bearer sts-token"));
        assert!(requests[1].ends_with(r#"{"lifetime":"600s","scope":["scope-a"]}"#));
    }

    #[tokio::test]
    async fn sts_failures_carry_the_status_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("subject");
        std::fs::write(&token_file, "jwt").unwrap();
        let (sts, _handle) = serve(vec![(400, r#"{"error":"invalid_grant"}"#.to_owned())]).await;
        let json = account_json(token_file.to_str().unwrap(), &sts, "");
        let account = ExternalAccount::from_json(json.as_bytes(), &profile(&["s"])).unwrap();

        assert_eq!(
            account.exchange().await.unwrap_err(),
            r#"credentials: status code 400: {"error":"invalid_grant"}"#
        );
    }

    #[test]
    fn configuration_errors_surface_at_construction() {
        assert_eq!(
            ExternalAccount::from_json(br#"{"type":"external_account"}"#, &profile(&[]))
                .err()
                .unwrap(),
            "externalaccount: Audience must be set"
        );
    }
}
