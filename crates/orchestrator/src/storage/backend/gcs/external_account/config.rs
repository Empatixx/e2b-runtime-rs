use std::collections::HashMap;

use serde::Deserialize;

const WORKFORCE_PREFIX: &str = "//iam.googleapis.com/locations/";
const WORKFORCE_POOLS: &str = "/workforcePools/";
const DEFAULT_UNIVERSE: &str = "googleapis.com";
const STS_URL: &str = "https://sts.UNIVERSE_DOMAIN/v1/token";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExternalAccountFile {
    pub client_id: String,
    pub client_secret: String,
    pub audience: String,
    pub subject_token_type: String,
    pub service_account_impersonation_url: String,
    pub token_url: String,
    pub credential_source: Option<CredentialSource>,
    pub service_account_impersonation: Option<ImpersonationInfo>,
    pub workforce_pool_user_project: String,
    pub universe_domain: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ImpersonationInfo {
    pub token_lifetime_seconds: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CredentialSource {
    pub file: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub executable: Option<ExecutableConfig>,
    pub certificate: Option<CertificateConfig>,
    pub environment_id: String,
    pub region_url: String,
    pub regional_cred_verification_url: String,
    pub imdsv2_session_token_url: String,
    pub format: Option<Format>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Format {
    pub r#type: String,
    pub subject_token_field_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ExecutableConfig {
    pub command: String,
    pub timeout_millis: i64,
    pub output_file: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CertificateConfig {
    pub trust_chain_path: String,
    pub use_default_certificate_config: bool,
    pub certificate_config_location: String,
}

impl ExternalAccountFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.audience.is_empty() {
            return Err("externalaccount: Audience must be set".to_owned());
        }
        if self.subject_token_type.is_empty() {
            return Err("externalaccount: Subject token type must be set".to_owned());
        }
        if !self.workforce_pool_user_project.is_empty() && !is_workforce_audience(&self.audience) {
            return Err(
                "externalaccount: workforce_pool_user_project should not be set for non-workforce pool credentials"
                    .to_owned(),
            );
        }
        match self.credential_source {
            Some(_) => Ok(()),
            None => Err(
                "externalaccount: one of CredentialSource, SubjectTokenProvider, or AwsSecurityCredentialsProvider must be set"
                    .to_owned(),
            ),
        }
    }

    pub fn resolved_token_url(&self) -> String {
        match (self.token_url.as_str(), self.universe_domain.as_str()) {
            ("", "") => STS_URL.replacen("UNIVERSE_DOMAIN", DEFAULT_UNIVERSE, 1),
            ("", universe) => STS_URL.replacen("UNIVERSE_DOMAIN", universe, 1),
            (url, _) => url.to_owned(),
        }
    }

    pub fn impersonation_lifetime_seconds(&self) -> i64 {
        self.service_account_impersonation
            .as_ref()
            .map_or(0, |info| info.token_lifetime_seconds)
    }
}

fn is_workforce_audience(audience: &str) -> bool {
    audience.find(WORKFORCE_PREFIX).is_some_and(|start| {
        let rest = &audience[start + WORKFORCE_PREFIX.len()..];
        rest.split_once(WORKFORCE_POOLS)
            .is_some_and(|(location, _)| !location.is_empty() && !location.contains('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(json: &str) -> ExternalAccountFile {
        ExternalAccountFile::parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn validation_reports_the_missing_pieces() {
        assert_eq!(
            file("{}").validate().unwrap_err(),
            "externalaccount: Audience must be set"
        );
        assert_eq!(
            file(r#"{"audience":"a"}"#).validate().unwrap_err(),
            "externalaccount: Subject token type must be set"
        );
        assert_eq!(
            file(r#"{"audience":"a","subject_token_type":"t","workforce_pool_user_project":"p"}"#)
                .validate()
                .unwrap_err(),
            "externalaccount: workforce_pool_user_project should not be set for non-workforce pool credentials"
        );
        assert_eq!(
            file(r#"{"audience":"a","subject_token_type":"t"}"#)
                .validate()
                .unwrap_err(),
            "externalaccount: one of CredentialSource, SubjectTokenProvider, or AwsSecurityCredentialsProvider must be set"
        );
        assert!(
            file(r#"{"audience":"//iam.googleapis.com/locations/global/workforcePools/p/providers/x","subject_token_type":"t","workforce_pool_user_project":"p","credential_source":{"file":"/f"}}"#)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn the_sts_url_follows_the_universe_domain() {
        assert_eq!(file("{}").resolved_token_url(), "https://sts.googleapis.com/v1/token");
        assert_eq!(
            file(r#"{"universe_domain":"example.com"}"#).resolved_token_url(),
            "https://sts.example.com/v1/token"
        );
        assert_eq!(file(r#"{"token_url":"http://sts"}"#).resolved_token_url(), "http://sts");
    }
}
