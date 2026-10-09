mod aws;
mod config;
mod executable;
mod sts;
mod subject;
mod x509;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use object_store::gcp::GcpCredential;
use object_store::{CredentialProvider, Error as StoreError};
use reqwest::Client;

use super::super::credential_cache::CredentialCache;
use config::ExternalAccountFile;
use subject::Subject;

const STORE: &str = "GoogleCloudStorage";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const EARLY_REFRESH: Duration = Duration::from_secs(165);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_TYPE: &str = "external_account";

pub struct ExternalAccount {
    file: ExternalAccountFile,
    subject: Subject,
    client: Client,
    scopes: Vec<String>,
}

#[derive(Debug)]
pub struct ExternalAccountCredentials {
    account: Arc<ExternalAccount>,
    cache: CredentialCache<GcpCredential>,
}

impl std::fmt::Debug for ExternalAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalAccount")
            .field("audience", &self.file.audience)
            .finish_non_exhaustive()
    }
}

pub fn is_external_account(json: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(json)
        .ok()
        .and_then(|value| value.get("type").and_then(|kind| kind.as_str().map(str::to_owned)))
        .is_some_and(|kind| kind == FILE_TYPE)
}

impl ExternalAccountCredentials {
    pub fn from_json(json: &[u8], scopes: &[&str]) -> Result<Self, String> {
        ExternalAccount::from_json(json, scopes).map(|account| Self {
            account: Arc::new(account),
            cache: CredentialCache::default(),
        })
    }
}

impl ExternalAccount {
    pub fn from_json(json: &[u8], scopes: &[&str]) -> Result<Self, String> {
        let file = ExternalAccountFile::parse(json)?;
        file.validate()?;
        let plain = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        let subject = Subject::from_file(&file, &plain)?;
        let client = match &subject {
            Subject::X509(x509) => x509.client()?,
            _ => plain,
        };
        Ok(Self {
            file,
            subject,
            client,
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        })
    }

    pub async fn token(&self) -> Result<(String, SystemTime), String> {
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
                    self.file.impersonation_lifetime_seconds(),
                )
                .await
            }
        }
    }
}

#[async_trait::async_trait]
impl CredentialProvider for ExternalAccountCredentials {
    type Credential = GcpCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<GcpCredential>> {
        self.cache
            .get_or_fetch(|| async {
                self.account
                    .token()
                    .await
                    .map(|(bearer, expiry)| (GcpCredential { bearer }, expiry.checked_sub(EARLY_REFRESH)))
                    .map_err(|source| StoreError::Generic {
                        store: STORE,
                        source: source.into(),
                    })
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::storage::backend::azure::testing::read_request;

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

    #[test]
    fn external_account_files_are_recognised_by_type() {
        assert!(is_external_account(br#"{"type":"external_account"}"#));
        assert!(!is_external_account(br#"{"type":"service_account"}"#));
        assert!(!is_external_account(b"nope"));
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
        let credentials = ExternalAccountCredentials::from_json(json.as_bytes(), &["scope-a", "scope-b"]).unwrap();

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
        let account = ExternalAccount::from_json(json.as_bytes(), &["scope-a"]).unwrap();

        let (token, _) = account.token().await.unwrap();
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
        let account = ExternalAccount::from_json(json.as_bytes(), &["s"]).unwrap();

        assert_eq!(
            account.token().await.unwrap_err(),
            r#"credentials: status code 400: {"error":"invalid_grant"}"#
        );
    }

    #[test]
    fn configuration_errors_surface_at_construction() {
        assert_eq!(
            ExternalAccount::from_json(br#"{"type":"external_account"}"#, &[])
                .err()
                .unwrap(),
            "externalaccount: Audience must be set"
        );
    }
}
