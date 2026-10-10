pub mod sse;

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use aws_config::{BehaviorVersion, ConfigLoader, SdkConfig};
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use object_store::aws::AwsCredential;
use object_store::{CredentialProvider, Error as StoreError};

use super::credential_cache::CredentialCache;
use tokio::sync::OnceCell;

const STORE: &str = "AwsDefaultChain";

#[derive(Debug, Clone)]
pub struct AwsDefaults {
    config: SdkConfig,
}

impl AwsDefaults {
    pub async fn load() -> Self {
        Self::from_loader(aws_config::defaults(BehaviorVersion::latest())).await
    }

    pub async fn from_loader(loader: ConfigLoader) -> Self {
        Self {
            config: loader.load().await,
        }
    }

    pub fn sdk_config(&self) -> &SdkConfig {
        &self.config
    }

    pub fn region(&self) -> Option<String> {
        self.config.region().map(|region| region.to_string())
    }

    pub async fn credentials(&self) -> Result<Credentials, String> {
        match self.config.credentials_provider() {
            Some(provider) => provider.provide_credentials().await.map_err(|e| e.to_string()),
            None => Err("no credentials provider is configured".to_owned()),
        }
    }

    pub fn blocking_region() -> Option<String> {
        super::blocking::run(async { Self::load().await.region() }).flatten()
    }
}

#[derive(Debug, Default)]
pub struct DefaultChainCredentials {
    defaults: OnceCell<AwsDefaults>,
    cache: CredentialCache<AwsCredential>,
}

impl DefaultChainCredentials {
    async fn fetch(&self) -> object_store::Result<(AwsCredential, Option<SystemTime>)> {
        let credentials = self
            .defaults
            .get_or_init(AwsDefaults::load)
            .await
            .credentials()
            .await
            .map_err(|source| StoreError::Generic {
                store: STORE,
                source: source.into(),
            })?;
        let credential = AwsCredential {
            key_id: credentials.access_key_id().to_owned(),
            secret_key: credentials.secret_access_key().to_owned(),
            token: credentials.session_token().map(str::to_owned),
        };
        Ok((credential, credentials.expiry()))
    }
}

#[async_trait]
impl CredentialProvider for DefaultChainCredentials {
    type Credential = AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<AwsCredential>> {
        self.cache.get_or_fetch(|| self.fetch()).await
    }
}

#[cfg(test)]
mod tests {
    use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};

    use super::*;

    fn defaults_from(credentials: &str, config: &str) -> impl std::future::Future<Output = AwsDefaults> {
        let dir = tempfile::tempdir().unwrap();
        let credentials_path = dir.path().join("credentials");
        let config_path = dir.path().join("config");
        std::fs::write(&credentials_path, credentials).unwrap();
        std::fs::write(&config_path, config).unwrap();
        let files = EnvConfigFiles::builder()
            .with_file(EnvConfigFileKind::Credentials, credentials_path)
            .with_file(EnvConfigFileKind::Config, config_path)
            .build();
        let loader = aws_config::defaults(BehaviorVersion::latest())
            .profile_name("e2b")
            .profile_files(files);
        async move {
            let _keep = dir;
            AwsDefaults::from_loader(loader).await
        }
    }

    #[tokio::test]
    async fn profiles_supply_credentials_and_region() {
        let defaults = defaults_from(
            "[e2b]\naws_access_key_id = AKIDPROFILE\naws_secret_access_key = secret\naws_session_token = token\n",
            "[profile e2b]\nregion = eu-west-3\n",
        )
        .await;

        let credentials = defaults.credentials().await.unwrap();

        assert_eq!(credentials.access_key_id(), "AKIDPROFILE");
        assert_eq!(credentials.secret_access_key(), "secret");
        assert_eq!(credentials.session_token(), Some("token"));
        assert_eq!(defaults.region().as_deref(), Some("eu-west-3"));
    }
}
