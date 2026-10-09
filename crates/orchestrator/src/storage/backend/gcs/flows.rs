mod authorized_user;
mod cached;
mod client;
mod compute;
mod external_account_user;
mod gdch;
mod impersonated;
mod response;
mod service_account;
mod service_key;

use std::sync::Arc;
use std::time::SystemTime;

use object_store::gcp::GcpCredential;
use object_store::{CredentialProvider, Error as StoreError};
use serde::Deserialize;

use super::external_account::ExternalAccount;
use super::universe::{client_universe_domain, validate_universe_domain};
pub use cached::CachedFlow;
pub use client::{default_client, request_error};
pub use compute::{ComputeFlow, Messages as ComputeMessages, on_gce};
pub use response::{Dialect, read_three_legged};

const STORE: &str = "GoogleCloudStorage";
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const UNIDENTIFIED: &str = "credentials: unsupported unidentified file type";
const ADC_SETUP_URL: &str = "https://cloud.google.com/docs/authentication/external/set-up-adc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowToken {
    pub value: String,
    pub expiry: Option<SystemTime>,
}

#[async_trait::async_trait]
pub trait TokenFlow: Send + Sync {
    async fn token(&self) -> Result<FlowToken, String>;

    fn universe_domain(&self) -> String {
        String::new()
    }

    fn is_compute(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone)]
pub struct FlowProfile {
    pub scopes: Vec<String>,
    pub self_signed_jwt: bool,
}

impl FlowProfile {
    pub fn new(scopes: &[&str], self_signed_jwt: bool) -> Self {
        Self {
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            self_signed_jwt,
        }
    }

    pub fn with_scopes(&self, scopes: &[&str]) -> Self {
        Self::new(scopes, self.self_signed_jwt)
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileType {
    r#type: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ClientCredentials {
    web: Option<RedirectConfig>,
    installed: Option<RedirectConfig>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RedirectConfig {
    redirect_uris: Vec<String>,
}

fn is_client_credentials(json: &[u8]) -> bool {
    serde_json::from_slice::<ClientCredentials>(json)
        .ok()
        .and_then(|creds| creds.web.or(creds.installed))
        .is_some_and(|config| !config.redirect_uris.is_empty())
}

pub fn build(json: &[u8], profile: &FlowProfile) -> Result<Arc<dyn TokenFlow>, String> {
    if is_client_credentials(json) {
        return Err("credentials: auth handler must be specified for this credential filetype".to_owned());
    }
    let kind = serde_json::from_slice::<FileType>(json)
        .map_err(|e| e.to_string())?
        .r#type;
    let flow: Arc<dyn TokenFlow> = match kind.as_str() {
        "" => return Err(UNIDENTIFIED.to_owned()),
        "service_account" => service_account::ServiceAccountFlow::from_json(json, profile)?,
        "authorized_user" => authorized_user::AuthorizedUserFlow::from_json(json)?,
        "external_account" => ExternalAccount::from_json(json, profile)?,
        "external_account_authorized_user" => external_account_user::ExternalAccountUserFlow::from_json(json)?,
        "gdch_service_account" => return gdch::from_json(json),
        "impersonated_service_account" => impersonated::ImpersonatedFlow::from_json(json, profile)?,
        other => {
            return Err(format!(
                "credentials: unsupported filetype {}",
                crate::go_strconv::quote(other)
            ));
        }
    };
    Ok(CachedFlow::new(flow))
}

pub fn compute(profile: &FlowProfile) -> Result<Arc<dyn TokenFlow>, String> {
    let flow: Arc<dyn TokenFlow> = ComputeFlow::new(&profile.scopes, ComputeMessages::CREDENTIALS)?;
    Ok(CachedFlow::new(flow))
}

pub fn missing_credentials() -> String {
    format!("credentials: could not find default credentials. See {ADC_SETUP_URL} for more information")
}

pub struct FlowCredentials {
    flow: Arc<dyn TokenFlow>,
}

impl std::fmt::Debug for FlowCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowCredentials").finish_non_exhaustive()
    }
}

impl FlowCredentials {
    pub fn new(flow: Arc<dyn TokenFlow>) -> Self {
        Self { flow }
    }

    pub fn from_json(json: &[u8], profile: &FlowProfile) -> Result<Self, String> {
        build(json, profile).map(Self::new)
    }
}

fn store_error(source: String) -> StoreError {
    StoreError::Generic {
        store: STORE,
        source: source.into(),
    }
}

#[async_trait::async_trait]
impl CredentialProvider for FlowCredentials {
    type Credential = GcpCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<GcpCredential>> {
        let token = self.flow.token().await.map_err(store_error)?;
        if !self.flow.is_compute() {
            validate_universe_domain(&client_universe_domain(), &self.flow.universe_domain()).map_err(store_error)?;
        }
        Ok(Arc::new(GcpCredential { bearer: token.value }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        universe: &'static str,
        compute: bool,
    }

    #[async_trait::async_trait]
    impl TokenFlow for Fixed {
        async fn token(&self) -> Result<FlowToken, String> {
            Ok(FlowToken {
                value: "bearer".to_owned(),
                expiry: None,
            })
        }

        fn universe_domain(&self) -> String {
            self.universe.to_owned()
        }

        fn is_compute(&self) -> bool {
            self.compute
        }
    }

    async fn credential_of(flow: Fixed) -> object_store::Result<Arc<GcpCredential>> {
        FlowCredentials::new(Arc::new(flow)).get_credential().await
    }

    #[tokio::test]
    async fn credentials_of_another_universe_are_refused_even_with_a_valid_token() {
        let error = credential_of(Fixed {
            universe: "example.com",
            compute: false,
        })
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains(
                "the configured universe domain (\"googleapis.com\") does not match the universe domain found in the credentials (\"example.com\")"
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn the_default_universe_and_compute_tokens_pass() {
        let plain = credential_of(Fixed {
            universe: "",
            compute: false,
        })
        .await
        .unwrap();
        let compute = credential_of(Fixed {
            universe: "example.com",
            compute: true,
        })
        .await
        .unwrap();

        assert_eq!(plain.bearer, "bearer");
        assert_eq!(compute.bearer, "bearer");
    }

    #[test]
    fn missing_credentials_point_at_the_adc_setup_page() {
        assert_eq!(
            missing_credentials(),
            "credentials: could not find default credentials. See https://cloud.google.com/docs/authentication/external/set-up-adc for more information"
        );
    }

    #[test]
    fn client_credentials_files_need_an_auth_handler() {
        let json = br#"{"installed":{"redirect_uris":["http://localhost"]}}"#;

        assert_eq!(
            build(json, &FlowProfile::new(&[], false)).err().unwrap(),
            "credentials: auth handler must be specified for this credential filetype"
        );
    }

    #[test]
    fn unknown_and_missing_file_types_are_named() {
        let profile = FlowProfile::new(&[], false);

        assert_eq!(
            build(b"{}", &profile).err().unwrap(),
            "credentials: unsupported unidentified file type"
        );
        assert_eq!(
            build(br#"{"type":"x"}"#, &profile).err().unwrap(),
            "credentials: unsupported filetype \"x\""
        );
    }
}
