mod impersonated;
mod jwt;
mod reuse;
mod user;

use std::future::Future;
use std::sync::Arc;

use object_store::gcp::GcpCredential;
use object_store::{CredentialProvider, Error as StoreError};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::adc::AdcFile;
use super::external_account::ExternalAccount;
use super::flows::{ComputeFlow, ComputeMessages, FlowProfile, TokenFlow, on_gce};
use reuse::ReuseFlow;

const STORE: &str = "GoogleCloudStorage";
const ADC_SETUP_URL: &str = "https://cloud.google.com/docs/authentication/external/set-up-adc";
const CREDENTIALS_ENV: &str = "GOOGLE_APPLICATION_CREDENTIALS";

#[derive(Deserialize, Default)]
#[serde(default)]
struct FileType {
    r#type: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ImpersonatedFile {
    service_account_impersonation_url: String,
    delegates: Vec<String>,
    source_credentials: Option<serde_json::Value>,
}

fn oauth2_flow(json: &[u8], scopes: &[String]) -> Result<Arc<dyn TokenFlow>, String> {
    let kind = serde_json::from_slice::<FileType>(json)
        .map_err(|e| e.to_string())?
        .r#type;
    let flow: Arc<dyn TokenFlow> = match kind.as_str() {
        "service_account" | "gdch_service_account" => jwt::JwtFlow::from_json(json, scopes)?,
        "authorized_user" => user::UserFlow::from_json(json)?,
        "external_account" => ExternalAccount::from_json(json, &profile(scopes))?,
        "external_account_authorized_user" => return super::flows::build(json, &profile(scopes)),
        "impersonated_service_account" => impersonated_flow(json, scopes)?,
        "" => return Err("missing 'type' field in credentials".to_owned()),
        other => return Err(format!("unknown credential type: {}", crate::go_strconv::quote(other))),
    };
    Ok(ReuseFlow::new(flow))
}

fn profile(scopes: &[String]) -> FlowProfile {
    FlowProfile {
        scopes: scopes.to_vec(),
        self_signed_jwt: false,
    }
}

fn impersonated_flow(json: &[u8], scopes: &[String]) -> Result<Arc<dyn TokenFlow>, String> {
    let file: ImpersonatedFile = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let source = match (
        file.source_credentials,
        file.service_account_impersonation_url.is_empty(),
    ) {
        (Some(source), false) => source.to_string(),
        _ => {
            return Err(
                "missing 'source_credentials' field or 'service_account_impersonation_url' in credentials".to_owned(),
            );
        }
    };
    let source = oauth2_flow(source.as_bytes(), scopes)?;
    let flow =
        impersonated::ImpersonatedFlow::new(file.service_account_impersonation_url, scopes, file.delegates, source)?;
    Ok(flow)
}

fn compute_flow(scopes: &[String]) -> Result<Arc<dyn TokenFlow>, String> {
    let flow: Arc<dyn TokenFlow> = ComputeFlow::new(scopes, ComputeMessages::OAUTH2)?;
    Ok(ReuseFlow::new(flow))
}

pub async fn find_default_credentials(scopes: &[String]) -> Result<Arc<dyn TokenFlow>, String> {
    find_default_with(&|name| std::env::var(name).ok(), scopes, on_gce()).await
}

async fn find_default_with(
    env: &(dyn Fn(&str) -> Option<String> + Sync),
    scopes: &[String],
    on_google_cloud: impl Future<Output = bool>,
) -> Result<Arc<dyn TokenFlow>, String> {
    match AdcFile::locate(env).map_err(environment_failure)? {
        Some(file) if file.from_environment => oauth2_flow(&file.bytes, scopes).map_err(environment_failure),
        Some(file) => oauth2_flow(&file.bytes, scopes),
        None if on_google_cloud.await => compute_flow(scopes),
        None => Err(format!(
            "google: could not find default credentials. See {ADC_SETUP_URL} for more information"
        )),
    }
}

fn environment_failure(error: String) -> String {
    format!("google: error getting credentials using {CREDENTIALS_ENV} environment variable: {error}")
}

pub struct MultipartCredentials {
    scopes: Vec<String>,
    flow: Mutex<Option<Arc<dyn TokenFlow>>>,
}

impl std::fmt::Debug for MultipartCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultipartCredentials").finish_non_exhaustive()
    }
}

impl MultipartCredentials {
    pub fn new(scopes: &[&str]) -> Self {
        Self {
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            flow: Mutex::new(None),
        }
    }

    async fn flow(&self) -> Result<Arc<dyn TokenFlow>, String> {
        let mut slot = self.flow.lock().await;
        if let Some(flow) = slot.as_ref() {
            return Ok(flow.clone());
        }
        let found = find_default_credentials(&self.scopes).await?;
        *slot = Some(found.clone());
        Ok(found)
    }
}

fn store_error(source: String) -> StoreError {
    StoreError::Generic {
        store: STORE,
        source: source.into(),
    }
}

#[async_trait::async_trait]
impl CredentialProvider for MultipartCredentials {
    type Credential = GcpCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<GcpCredential>> {
        let flow = self
            .flow()
            .await
            .map_err(|e| store_error(format!("failed to get credentials: {e}")))?;
        let token = flow
            .token()
            .await
            .map_err(|e| store_error(format!("failed to get token: {e}")))?;
        Ok(Arc::new(GcpCredential { bearer: token.value }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> Vec<String> {
        vec!["https://www.googleapis.com/auth/cloud-platform".to_owned()]
    }

    #[test]
    fn the_file_type_decides_the_flow_with_x_oauth2_wording() {
        assert_eq!(
            oauth2_flow(b"{}", &scopes()).err().unwrap(),
            "missing 'type' field in credentials"
        );
        assert_eq!(
            oauth2_flow(br#"{"type":"nope"}"#, &scopes()).err().unwrap(),
            "unknown credential type: \"nope\""
        );
        assert_eq!(
            oauth2_flow(br#"{"type":"impersonated_service_account"}"#, &scopes())
                .err()
                .unwrap(),
            "missing 'source_credentials' field or 'service_account_impersonation_url' in credentials"
        );
    }

    #[test]
    fn service_accounts_authorized_users_and_impersonation_build_without_the_network() {
        let source = r#"{"type":"authorized_user","client_id":"a","client_secret":"b","refresh_token":"c"}"#;
        let impersonated = format!(
            r#"{{"type":"impersonated_service_account","service_account_impersonation_url":"u","source_credentials":{source}}}"#
        );

        assert!(oauth2_flow(source.as_bytes(), &scopes()).is_ok());
        assert!(oauth2_flow(impersonated.as_bytes(), &scopes()).is_ok());
        assert!(oauth2_flow(br#"{"type":"service_account"}"#, &scopes()).is_ok());
    }

    #[test]
    fn an_unreadable_environment_file_is_wrapped_like_x_oauth2() {
        assert_eq!(
            environment_failure("open /x: no such file or directory".to_owned()),
            "google: error getting credentials using GOOGLE_APPLICATION_CREDENTIALS environment variable: open /x: no such file or directory"
        );
    }

    fn env_of(home: &std::path::Path, credentials: Option<&std::path::Path>) -> impl Fn(&str) -> Option<String> {
        let home = home.display().to_string();
        let credentials = credentials.map(|path| path.display().to_string());
        move |name| match name {
            "HOME" => Some(home.clone()),
            "GOOGLE_APPLICATION_CREDENTIALS" => credentials.clone(),
            _ => None,
        }
    }

    #[tokio::test]
    async fn the_chain_ends_with_the_google_error_when_nothing_is_found() {
        let home = tempfile::tempdir().unwrap();

        let error = find_default_with(&env_of(home.path(), None), &scopes(), async { false })
            .await
            .err()
            .unwrap();

        assert_eq!(
            error,
            "google: could not find default credentials. See https://cloud.google.com/docs/authentication/external/set-up-adc for more information"
        );
    }

    #[tokio::test]
    async fn an_environment_file_failure_is_wrapped_but_a_well_known_one_is_not() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/gcloud")).unwrap();
        std::fs::write(
            home.path().join(".config/gcloud/application_default_credentials.json"),
            b"{}",
        )
        .unwrap();
        let broken = home.path().join("broken.json");
        std::fs::write(&broken, b"{}").unwrap();

        let well_known = find_default_with(&env_of(home.path(), None), &scopes(), async { false }).await;
        let environment = find_default_with(&env_of(home.path(), Some(&broken)), &scopes(), async { false }).await;

        assert_eq!(well_known.err().unwrap(), "missing 'type' field in credentials");
        assert_eq!(
            environment.err().unwrap(),
            "google: error getting credentials using GOOGLE_APPLICATION_CREDENTIALS environment variable: missing 'type' field in credentials"
        );
    }

    #[tokio::test]
    async fn the_credential_provider_names_the_failing_step() {
        let credentials = MultipartCredentials::new(&["scope"]);
        let flow: Arc<dyn TokenFlow> = Arc::new(Failing);
        *credentials.flow.lock().await = Some(flow);

        let error = credentials.get_credential().await.unwrap_err();

        assert!(error.to_string().contains("failed to get token: refused"), "{error}");
    }

    struct Failing;

    #[async_trait::async_trait]
    impl TokenFlow for Failing {
        async fn token(&self) -> Result<super::super::flows::FlowToken, String> {
            Err("refused".to_owned())
        }
    }
}
