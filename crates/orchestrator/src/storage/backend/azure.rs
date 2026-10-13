mod aad;
pub mod azcore;
mod azure_cli;
mod azure_developer_cli;
mod client_certificate;
mod client_secret;
mod connection_string;
pub mod default_credential;
mod delegation;
mod dev_tool;
mod managed_identity;
mod msal;
mod powershell;
mod retry;
mod sas;
mod tenant;
#[cfg(test)]
pub mod testing;
mod token_source;
mod username_password;
mod workload_identity;
mod ws_trust;
mod xml;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use object_store::azure::{AzureCredential, MicrosoftAzure, MicrosoftAzureBuilder};
use object_store::{CredentialProvider, Error as StoreError};

use super::credential_cache::CredentialCache;
use crate::go_url::escape::{Mode, should_escape};
use crate::go_url::query::Query;
use crate::storage::spec::StorageSpec;
use connection_string::{ConnectionString, SharedKey};
use default_credential::DefaultAzureCredential;
use delegation::DelegationRequest;
use sas::{BlobSas, SasKey};
use token_source::TokenRequest;

const STORAGE_SCOPE: &str = "https://storage.azure.com/.default";
const CACHE_MARGIN: Duration = Duration::from_secs(60);
const STORE: &str = "MicrosoftAzure";
const SIGNING_UNSUPPORTED: &str = "signed upload URLs are not supported by this storage provider";

pub struct AzureEnv<'a> {
    pub connection_string: Option<&'a str>,
    pub account_name: Option<&'a str>,
    pub account_key: Option<&'a str>,
}

pub struct AzureBackend {
    pub store: MicrosoftAzure,
    pub signer: AzureSigner,
}

#[derive(Clone)]
pub struct AzureSigner {
    container: String,
    service_url: String,
    auth: SigningAuth,
}

#[derive(Clone)]
enum SigningAuth {
    SharedKey(SharedKey),
    Delegation {
        account: String,
        credential: Arc<DefaultAzureCredential>,
        client: reqwest::Client,
    },
    Unsupported,
}

#[derive(Debug)]
struct StorageTokens {
    credential: Arc<DefaultAzureCredential>,
    cache: CredentialCache<AzureCredential>,
}

#[async_trait::async_trait]
impl CredentialProvider for StorageTokens {
    type Credential = AzureCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<AzureCredential>> {
        self.cache
            .get_or_fetch(|| async {
                self.credential
                    .token_for(&storage_request())
                    .await
                    .map(|token| {
                        let fresh_until = token.refresh_instant() + CACHE_MARGIN;
                        (AzureCredential::BearerToken(token.secret), Some(fresh_until))
                    })
                    .map_err(|source| StoreError::Generic {
                        store: STORE,
                        source: source.into(),
                    })
            })
            .await
    }
}

fn storage_request() -> TokenRequest<'static> {
    TokenRequest::new(STORAGE_SCOPE).with_cae()
}

pub fn open(spec: &StorageSpec, env: AzureEnv) -> Result<AzureBackend, String> {
    match env.connection_string.filter(|value| !value.is_empty()) {
        Some(connection_string) => from_connection_string(spec, connection_string),
        None => from_account(spec, env),
    }
}

fn from_connection_string(spec: &StorageSpec, text: &str) -> Result<AzureBackend, String> {
    let parsed = ConnectionString::parse(text)
        .map_err(|e| format!("failed to create Azure client from connection string: {e}"))?;
    let builder = base_builder(spec, &parsed.service_url);
    let builder = match (&parsed.shared_key, &parsed.shared_access_signature) {
        (Some(key), _) => builder
            .with_account(&key.account)
            .with_access_key(STANDARD.encode(&key.key)),
        (None, Some(signature)) => builder
            .with_account(parsed.account_from_host())
            .with_sas_authorization(sas_pairs(signature)),
        (None, None) => builder,
    };
    let store = build(builder)?;
    let signing_key = parsed
        .shared_key
        .filter(|key| !key.account.is_empty() && !key.key.is_empty());
    if signing_key.is_none() {
        tracing::warn!(
            container = %spec.bucket,
            "Azure storage cannot sign upload URLs with a SAS-only connection string: template layer-file uploads will fail on a cache miss"
        );
    }
    Ok(AzureBackend {
        store,
        signer: AzureSigner {
            container: spec.bucket.clone(),
            service_url: parsed.service_url,
            auth: signing_key.map_or(SigningAuth::Unsupported, SigningAuth::SharedKey),
        },
    })
}

fn from_account(spec: &StorageSpec, env: AzureEnv) -> Result<AzureBackend, String> {
    let account = env
        .account_name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "AZURE_STORAGE_ACCOUNT_NAME environment variable is not set".to_owned())?;
    let service_url = format!("https://{account}.blob.core.windows.net/");
    let builder = base_builder(spec, &service_url).with_account(account);
    match env.account_key.filter(|key| !key.is_empty()) {
        Some(key) => with_account_key(spec, builder, account, key, service_url),
        None => with_default_credential(spec, builder, account, service_url),
    }
}

fn with_account_key(
    spec: &StorageSpec,
    builder: MicrosoftAzureBuilder,
    account: &str,
    key: &str,
    service_url: String,
) -> Result<AzureBackend, String> {
    let decoded = STANDARD.decode(key).map_err(|e| {
        format!(
            "failed to create Azure shared key credential: decode account key: {}",
            crate::builder::oci::ecr::base64_error(key, e)
        )
    })?;
    Ok(AzureBackend {
        store: build(builder.with_access_key(key))?,
        signer: AzureSigner {
            container: spec.bucket.clone(),
            service_url,
            auth: SigningAuth::SharedKey(SharedKey {
                account: account.to_owned(),
                key: decoded,
            }),
        },
    })
}

fn with_default_credential(
    spec: &StorageSpec,
    builder: MicrosoftAzureBuilder,
    account: &str,
    service_url: String,
) -> Result<AzureBackend, String> {
    let credential = DefaultAzureCredential::new()
        .map(Arc::new)
        .map_err(|e| format!("failed to create Azure default credential: {e}"))?;
    let provider = StorageTokens {
        credential: credential.clone(),
        cache: CredentialCache::default(),
    };
    Ok(AzureBackend {
        store: build(builder.with_credentials(Arc::new(provider)))?,
        signer: AzureSigner {
            container: spec.bucket.clone(),
            service_url,
            auth: SigningAuth::Delegation {
                account: account.to_owned(),
                credential,
                client: azcore::pipeline_client(),
            },
        },
    })
}

fn base_builder(spec: &StorageSpec, service_url: &str) -> MicrosoftAzureBuilder {
    MicrosoftAzureBuilder::new()
        .with_container_name(&spec.bucket)
        .with_endpoint(service_url.trim_end_matches('/').to_owned())
        .with_allow_http(service_url.starts_with("http://"))
}

fn build(builder: MicrosoftAzureBuilder) -> Result<MicrosoftAzure, String> {
    builder
        .build()
        .map_err(|e| format!("failed to create Azure client: {e}"))
}

fn sas_pairs(signature: &str) -> Vec<(String, String)> {
    Query::parse(signature.trim_start_matches('?'))
        .values
        .into_iter()
        .flat_map(|(key, values)| values.into_iter().map(move |value| (key.clone(), value)))
        .collect()
}

impl AzureSigner {
    pub async fn upload_url(&self, path: &str, ttl: Duration) -> Result<String, String> {
        let blob_url = self.blob_url(path);
        let sas = BlobSas::issued_now(
            &self.account(),
            &self.container,
            path,
            blob_url.starts_with("https://"),
            ttl,
        );
        let query = match &self.auth {
            SigningAuth::SharedKey(key) => sas.query(SasKey::Account(&key.key)),
            SigningAuth::Delegation { credential, client, .. } => {
                let key = self
                    .delegation_key(&sas, credential, client)
                    .await
                    .map_err(|e| format!("failed to get Azure user delegation key for {path:?}: {e}"))?;
                sas.query(SasKey::Delegation(&key))
            }
            SigningAuth::Unsupported => return Err(self.unsupported(path)),
        };
        Ok(format!("{blob_url}?{query}"))
    }

    async fn delegation_key(
        &self,
        sas: &BlobSas,
        credential: &DefaultAzureCredential,
        client: &reqwest::Client,
    ) -> Result<delegation::UserDelegationKey, String> {
        let token = credential.token_for(&storage_request()).await?;
        DelegationRequest {
            service_url: &self.service_url,
            bearer: &token.secret,
            start: sas.start,
            expiry: sas.expiry,
        }
        .fetch(client)
        .await
    }

    fn account(&self) -> String {
        match &self.auth {
            SigningAuth::SharedKey(key) => key.account.clone(),
            SigningAuth::Delegation { account, .. } => account.clone(),
            SigningAuth::Unsupported => String::new(),
        }
    }

    fn blob_url(&self, path: &str) -> String {
        format!(
            "{}/{}/{}",
            self.service_url.trim_end_matches('/'),
            self.container,
            path_escape(path)
        )
    }

    fn unsupported(&self, path: &str) -> String {
        format!(
            "{SIGNING_UNSUPPORTED}: Azure ({path:?}) needs AZURE_STORAGE_ACCOUNT_KEY, an AccountKey in AZURE_STORAGE_CONNECTION_STRING, or a credential allowed to fetch a user delegation key"
        )
    }
}

fn path_escape(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| match escapes_in_segment(byte) {
            true => format!("%{byte:02X}"),
            false => (byte as char).to_string(),
        })
        .collect()
}

fn escapes_in_segment(byte: u8) -> bool {
    matches!(byte, b'/' | b';' | b',') || should_escape(byte, Mode::Path)
}

#[cfg(test)]
mod tests {
    use crate::storage::spec::Provider;

    use super::*;

    fn spec() -> StorageSpec {
        StorageSpec {
            provider: Provider::Azure,
            bucket: "box".to_owned(),
            ..StorageSpec::local("")
        }
    }

    fn env<'a>(
        connection_string: Option<&'a str>,
        account_name: Option<&'a str>,
        account_key: Option<&'a str>,
    ) -> AzureEnv<'a> {
        AzureEnv {
            connection_string,
            account_name,
            account_key,
        }
    }

    fn opened(env: AzureEnv) -> AzureBackend {
        open(&spec(), env).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn an_account_name_is_required_without_a_connection_string() {
        let error = open(&spec(), env(None, None, None)).err().unwrap();

        assert_eq!(error, "AZURE_STORAGE_ACCOUNT_NAME environment variable is not set");
    }

    #[test]
    fn broken_connection_strings_carry_the_client_prefix() {
        let error = open(&spec(), env(Some("AccountName=a"), None, None)).err().unwrap();

        assert_eq!(
            error,
            "failed to create Azure client from connection string: connection string needs either AccountKey or SharedAccessSignature"
        );
    }

    #[test]
    fn a_bad_account_key_is_a_shared_key_error() {
        let error = open(&spec(), env(None, Some("acct"), Some("a*b="))).err().unwrap();

        assert_eq!(
            error,
            "failed to create Azure shared key credential: decode account key: illegal base64 data at input byte 1"
        );
    }

    #[tokio::test]
    async fn shared_key_uploads_are_signed_locally_with_the_escaped_blob_url() {
        let backend = opened(env(None, Some("acct"), Some("a2V5")));

        let url = backend
            .signer
            .upload_url("templates/a b.tar", Duration::from_secs(600))
            .await
            .unwrap();

        assert!(url.starts_with("https://acct.blob.core.windows.net/box/templates%2Fa%20b.tar?se="));
        assert!(url.contains("&sp=cw&spr=https&sr=b&st="));
        assert!(url.ends_with("&sv=2026-06-06"));
    }

    #[tokio::test]
    async fn connection_strings_with_a_key_sign_over_their_endpoint() {
        let backend = opened(env(
            Some("AccountName=devstoreaccount1;AccountKey=a2V5;BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1"),
            None,
            None,
        ));

        let url = backend.signer.upload_url("p", Duration::from_secs(60)).await.unwrap();

        assert!(url.starts_with("http://127.0.0.1:10000/devstoreaccount1/box/p?se="));
        assert!(url.contains("&spr=https%2Chttp&"));
    }

    #[tokio::test]
    async fn sas_only_connection_strings_cannot_sign_uploads() {
        let backend = opened(env(
            Some("BlobEndpoint=https://acct.blob.core.windows.net;SharedAccessSignature=sv=1&sig=x"),
            None,
            None,
        ));

        let error = backend
            .signer
            .upload_url("t/layer.tar", Duration::from_secs(60))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "signed upload URLs are not supported by this storage provider: Azure (\"t/layer.tar\") needs AZURE_STORAGE_ACCOUNT_KEY, an AccountKey in AZURE_STORAGE_CONNECTION_STRING, or a credential allowed to fetch a user delegation key"
        );
    }

    #[tokio::test]
    async fn token_credentials_sign_with_a_user_delegation_key() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let xml = "<UserDelegationKey><SignedOid>oid</SignedOid><SignedTid>tid</SignedTid><SignedStart>2026-01-02T03:04:05Z</SignedStart><SignedExpiry>2026-01-03T03:04:05Z</SignedExpiry><SignedService>b</SignedService><SignedVersion>2026-06-06</SignedVersion><Value>c2VjcmV0</Value></UserDelegationKey>";
        tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            testing::read_request(&mut connection).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{xml}",
                xml.len()
            );
            connection.write_all(response.as_bytes()).await.unwrap();
        });
        let signer = AzureSigner {
            container: "box".to_owned(),
            service_url: format!("http://{address}/"),
            auth: SigningAuth::Delegation {
                account: "acct".to_owned(),
                credential: Arc::new(DefaultAzureCredential::with_static_token("aad")),
                client: reqwest::Client::new(),
            },
        };

        let url = signer.upload_url("p", Duration::from_secs(60)).await.unwrap();

        assert!(url.starts_with(&format!("http://{address}/box/p?se=")));
        assert!(url.contains("&skoid=oid&sks=b&skt=2026-01-02T03%3A04%3A05Z&sktid=tid&skv=2026-06-06&"));
    }

    #[tokio::test]
    async fn a_refused_delegation_key_names_the_blob() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            testing::read_request(&mut connection).await;
            connection
                .write_all(b"HTTP/1.1 403 Forbidden\r\nx-ms-error-code: AuthorizationPermissionMismatch\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let signer = AzureSigner {
            container: "box".to_owned(),
            service_url: format!("http://{address}/"),
            auth: SigningAuth::Delegation {
                account: "acct".to_owned(),
                credential: Arc::new(DefaultAzureCredential::with_static_token("aad")),
                client: reqwest::Client::new(),
            },
        };

        let error = signer.upload_url("t/a.tar", Duration::from_secs(60)).await.unwrap_err();

        assert!(error.starts_with("failed to get Azure user delegation key for \"t/a.tar\": POST http://"));
        assert!(error.contains("RESPONSE 403: 403 Forbidden\nERROR CODE: AuthorizationPermissionMismatch\n"));
    }

    #[test]
    fn blob_names_are_escaped_like_path_escape() {
        assert_eq!(path_escape("a/b c;d,e?f"), "a%2Fb%20c%3Bd%2Ce%3Ff");
        assert_eq!(
            path_escape("x-y_z.~$&+:=@!()*\u{e9}"),
            "x-y_z.~$&+:=@%21%28%29%2A%C3%A9"
        );
    }
}
