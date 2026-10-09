use std::future::Future;
use std::sync::Arc;

use serde::Deserialize;

use super::adc::AdcFile;
use super::flows::{FlowProfile, TokenFlow, build, compute, missing_credentials, on_gce};

const QUOTA_PROJECT_ENV: &str = "GOOGLE_CLOUD_QUOTA_PROJECT";

pub struct Detected {
    pub flow: Arc<dyn TokenFlow>,
    pub quota_project: Option<String>,
}

pub async fn detect_default(profile: FlowProfile) -> Result<Detected, String> {
    detect_with(&|name| std::env::var(name).ok(), &profile, on_gce()).await
}

async fn detect_with(
    env: &(dyn Fn(&str) -> Option<String> + Sync),
    profile: &FlowProfile,
    on_google_cloud: impl Future<Output = bool>,
) -> Result<Detected, String> {
    match AdcFile::locate(env)? {
        Some(file) => Ok(Detected {
            flow: build(&file.bytes, profile)?,
            quota_project: quota_project(Some(&file.bytes)),
        }),
        None if on_google_cloud.await => Ok(Detected {
            flow: compute(profile)?,
            quota_project: quota_project(None),
        }),
        None => Err(missing_credentials()),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct QuotaFile {
    quota_project_id: String,
}

pub fn quota_project(json: Option<&[u8]>) -> Option<String> {
    quota_project_from(std::env::var(QUOTA_PROJECT_ENV).ok(), json)
}

fn quota_project_from(from_environment: Option<String>, json: Option<&[u8]>) -> Option<String> {
    from_environment.filter(|project| !project.is_empty()).or_else(|| {
        json.and_then(|bytes| serde_json::from_slice::<QuotaFile>(bytes).ok())
            .map(|file| file.quota_project_id)
            .filter(|project| !project.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quota_project_comes_from_the_environment_before_the_file() {
        let file = br#"{"quota_project_id":"from-file"}"#;

        assert_eq!(
            quota_project_from(Some("from-env".to_owned()), Some(file)).as_deref(),
            Some("from-env")
        );
        assert_eq!(quota_project_from(None, Some(file)).as_deref(), Some("from-file"));
        assert_eq!(
            quota_project_from(Some(String::new()), Some(file)).as_deref(),
            Some("from-file")
        );
        assert_eq!(quota_project_from(None, None), None);
        assert_eq!(quota_project_from(None, Some(b"not json")), None);
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
    async fn without_files_and_outside_google_cloud_the_sdk_error_comes_back() {
        let home = tempfile::tempdir().unwrap();

        let error = detect_with(&env_of(home.path(), None), &FlowProfile::new(&[], true), async {
            false
        })
        .await
        .err()
        .unwrap();

        assert_eq!(
            error,
            "credentials: could not find default credentials. See https://cloud.google.com/docs/authentication/external/set-up-adc for more information"
        );
    }

    #[tokio::test]
    async fn on_google_cloud_the_compute_flow_takes_over_and_the_probe_runs_only_then() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("c.json");
        std::fs::write(
            &file,
            br#"{"type":"authorized_user","client_id":"a","client_secret":"b","refresh_token":"c"}"#,
        )
        .unwrap();

        let from_file = detect_with(&env_of(home.path(), Some(&file)), &FlowProfile::new(&[], true), async {
            panic!("the metadata probe must not run when a file exists")
        })
        .await;
        let from_compute = detect_with(&env_of(home.path(), None), &FlowProfile::new(&[], true), async { true }).await;

        assert!(from_file.is_ok());
        assert!(from_compute.unwrap().flow.is_compute());
    }

    #[tokio::test]
    async fn the_grpc_profile_signs_service_accounts_locally_with_all_five_scopes() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("sa.json");
        let account = serde_json::json!({
            "type": "service_account",
            "client_email": "sa@p.iam",
            "private_key": include_str!("test_key.pem"),
            "private_key_id": "kid",
            "quota_project_id": "billing-project"
        });
        std::fs::write(&file, account.to_string()).unwrap();
        let scopes = ["https://a", "https://b"];

        let detected = detect_with(
            &env_of(home.path(), Some(&file)),
            &FlowProfile::new(&scopes, true),
            async { false },
        )
        .await
        .unwrap();
        let token = detected.flow.token().await.unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.value.split('.').nth(1).unwrap()).unwrap()).unwrap();

        assert_eq!(claims["scope"], "https://a https://b");
        assert_eq!(claims["aud"], "");
        assert_eq!(detected.quota_project.as_deref(), Some("billing-project"));
    }
}
