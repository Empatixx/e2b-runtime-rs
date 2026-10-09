mod authorized_user;
mod external_account_user;
mod gdch;
mod impersonated;
mod response;
mod service_account;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use object_store::gcp::GcpCredential;
use object_store::{CredentialProvider, Error as StoreError};
use serde::Deserialize;

use super::super::credential_cache::CredentialCache;
use super::external_account::ExternalAccount;

const STORE: &str = "GoogleCloudStorage";
const EARLY_REFRESH: Duration = Duration::from_secs(165);
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const UNIDENTIFIED: &str = "credentials: unsupported unidentified file type";

#[async_trait::async_trait]
pub trait TokenFlow: Send + Sync {
    async fn token(&self) -> Result<(String, Option<SystemTime>), String>;
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileType {
    r#type: String,
}

pub fn build(json: &[u8], scopes: &[&str]) -> Result<Arc<dyn TokenFlow>, String> {
    let kind = serde_json::from_slice::<FileType>(json)
        .map_err(|e| e.to_string())?
        .r#type;
    match kind.as_str() {
        "" => Err(UNIDENTIFIED.to_owned()),
        "service_account" => service_account::ServiceAccountFlow::from_json(json, scopes).map(|flow| flow as _),
        "authorized_user" => authorized_user::AuthorizedUserFlow::from_json(json).map(|flow| flow as _),
        "external_account" => ExternalAccount::from_json(json, scopes).map(|flow| Arc::new(flow) as _),
        "external_account_authorized_user" => {
            external_account_user::ExternalAccountUserFlow::from_json(json).map(|flow| flow as _)
        }
        "gdch_service_account" => gdch::from_json(json),
        "impersonated_service_account" => impersonated::ImpersonatedFlow::from_json(json, scopes).map(|flow| flow as _),
        other => Err(format!(
            "credentials: unsupported filetype {}",
            crate::go_strconv::quote(other)
        )),
    }
}

#[derive(Debug)]
pub struct FlowCredentials {
    flow: Arc<dyn TokenFlow>,
    cache: CredentialCache<GcpCredential>,
}

impl std::fmt::Debug for dyn TokenFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenFlow")
    }
}

impl FlowCredentials {
    pub fn from_json(json: &[u8], scopes: &[&str]) -> Result<Self, String> {
        build(json, scopes).map(Self::new)
    }

    pub fn new(flow: Arc<dyn TokenFlow>) -> Self {
        Self {
            flow,
            cache: CredentialCache::default(),
        }
    }
}

#[async_trait::async_trait]
impl CredentialProvider for FlowCredentials {
    type Credential = GcpCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<GcpCredential>> {
        self.cache
            .get_or_fetch(|| async {
                self.flow
                    .token()
                    .await
                    .map(|(bearer, expiry)| {
                        (
                            GcpCredential { bearer },
                            expiry.map(|expiry| expiry.checked_sub(EARLY_REFRESH).unwrap_or(expiry)),
                        )
                    })
                    .map_err(|source| StoreError::Generic {
                        store: STORE,
                        source: source.into(),
                    })
            })
            .await
    }
}
