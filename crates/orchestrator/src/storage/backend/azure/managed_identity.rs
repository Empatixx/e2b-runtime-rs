use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use azure_core::credentials::TokenCredential;
use azure_core::error::ErrorKind;
use azure_identity::{ManagedIdentityCredential, ManagedIdentityCredentialOptions, UserAssignedId};
use reqwest::StatusCode;

use super::azcore::{self, AuthenticationFailure};
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource, expiry};

const CREDENTIAL: &str = "ManagedIdentityCredential";
const IMDS_ENDPOINT: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const PROBE_TIMED_OUT: &str =
    "managed identity timed out. See https://aka.ms/azsdk/go/identity/troubleshoot#dac for more information";

pub struct ManagedIdentitySource {
    credential: Arc<ManagedIdentityCredential>,
    imds: bool,
    user_assigned: bool,
    chained: bool,
    probe: AtomicBool,
    endpoint: String,
}

pub struct ManagedIdentitySettings<'a> {
    pub client_id: Option<String>,
    pub chained: bool,
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

impl ManagedIdentitySource {
    pub fn new(settings: ManagedIdentitySettings) -> Result<Self, String> {
        let imds = uses_imds(settings.env);
        let user_assigned = settings.client_id.is_some();
        let options = ManagedIdentityCredentialOptions {
            user_assigned_id: settings.client_id.map(UserAssignedId::ClientId),
            ..ManagedIdentityCredentialOptions::default()
        };
        let credential = ManagedIdentityCredential::new(Some(options)).map_err(|e| e.to_string())?;
        Ok(Self {
            credential,
            imds,
            user_assigned,
            chained: settings.chained,
            probe: AtomicBool::new(imds && settings.chained),
            endpoint: endpoint(imds, settings.env),
        })
    }

    async fn probe_imds(&self) -> Result<(), SourceError> {
        if !self.probe.load(Ordering::SeqCst) {
            return Ok(());
        }
        azcore::pipeline_client()
            .get(IMDS_ENDPOINT)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .map_err(|e| SourceError::Unavailable(format!("{CREDENTIAL}: {}", probe_failure(&e))))?;
        self.probe.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn classify(&self, error: &azure_core::Error) -> SourceError {
        match error.kind() {
            ErrorKind::HttpResponse {
                status, raw_response, ..
            } => {
                let body = raw_response
                    .as_ref()
                    .map_or_else(String::new, |raw| String::from_utf8_lossy(raw.body()).into_owned());
                self.http_failure(u16::from(*status), &body)
            }
            ErrorKind::DataConversion if self.imds && self.chained => {
                SourceError::Unavailable(format!("{CREDENTIAL}: {error}"))
            }
            _ => SourceError::Failed(format!("{CREDENTIAL}: {error}")),
        }
    }

    fn http_failure(&self, status: u16, body: &str) -> SourceError {
        match (self.imds, status) {
            (true, 400) if !self.user_assigned => {
                SourceError::Unavailable(format!("{CREDENTIAL}: {}", system_assigned_failure(body)))
            }
            (true, 403) if body.contains("unreachable") => SourceError::Unavailable(format!(
                "{CREDENTIAL}: unexpected response {}",
                crate::go_strconv::quote(body)
            )),
            (true, 400) => self.rendered("the requested identity isn't assigned to this resource", status, body),
            _ => self.rendered("", status, body),
        }
    }

    fn rendered(&self, message: &str, status: u16, body: &str) -> SourceError {
        SourceError::Failed(
            AuthenticationFailure {
                credential: CREDENTIAL,
                message,
                method: "GET",
                url: &self.endpoint,
                status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                body,
            }
            .text(),
        )
    }
}

fn system_assigned_failure(body: &str) -> String {
    match body.is_empty() {
        true => "failed to authenticate a system assigned identity".to_owned(),
        false => format!("failed to authenticate a system assigned identity. The endpoint responded with {body}"),
    }
}

fn probe_failure(error: &reqwest::Error) -> String {
    match error.is_timeout() {
        true => PROBE_TIMED_OUT.to_owned(),
        false => format!("Get {}: {error}", crate::go_strconv::quote(IMDS_ENDPOINT)),
    }
}

fn uses_imds(env: &dyn Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| env(name).is_some();
    match (set("IDENTITY_ENDPOINT"), set("MSI_ENDPOINT")) {
        (true, _) => !(set("IDENTITY_HEADER") || set("IMDS_ENDPOINT")),
        (false, true) => false,
        (false, false) => true,
    }
}

fn endpoint(imds: bool, env: &dyn Fn(&str) -> Option<String>) -> String {
    match imds {
        true => IMDS_ENDPOINT.to_owned(),
        false => env("IDENTITY_ENDPOINT")
            .or_else(|| env("MSI_ENDPOINT"))
            .unwrap_or_default(),
    }
}

#[async_trait::async_trait]
impl TokenSource for ManagedIdentitySource {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        self.probe_imds().await?;
        self.credential
            .get_token(&[request.scope], None)
            .await
            .map(|token| {
                BearerToken::without_refresh(
                    token.token.secret().to_owned(),
                    expiry(token.expires_on.unix_timestamp()),
                )
            })
            .map_err(|e| self.classify(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(imds: bool, user_assigned: bool) -> ManagedIdentitySource {
        ManagedIdentitySource {
            credential: ManagedIdentityCredential::new(None).unwrap(),
            imds,
            user_assigned,
            chained: true,
            probe: AtomicBool::new(false),
            endpoint: IMDS_ENDPOINT.to_owned(),
        }
    }

    #[test]
    fn imds_is_used_unless_another_identity_endpoint_is_configured() {
        let env = |pairs: &'static [&'static str]| move |name: &str| pairs.contains(&name).then(|| "x".to_owned());

        assert!(uses_imds(&env(&[])));
        assert!(!uses_imds(&env(&["IDENTITY_ENDPOINT", "IDENTITY_HEADER"])));
        assert!(!uses_imds(&env(&["IDENTITY_ENDPOINT", "IMDS_ENDPOINT"])));
        assert!(uses_imds(&env(&["IDENTITY_ENDPOINT"])));
        assert!(!uses_imds(&env(&["MSI_ENDPOINT"])));
    }

    #[test]
    fn a_system_assigned_identity_that_imds_rejects_lets_the_chain_move_on() {
        assert_eq!(
            source(true, false).http_failure(400, "nope"),
            SourceError::Unavailable(
                "ManagedIdentityCredential: failed to authenticate a system assigned identity. The endpoint responded with nope".to_owned()
            )
        );
        assert_eq!(
            source(true, false).http_failure(400, ""),
            SourceError::Unavailable(
                "ManagedIdentityCredential: failed to authenticate a system assigned identity".to_owned()
            )
        );
    }

    #[test]
    fn a_proxy_answering_forbidden_unreachable_lets_the_chain_move_on() {
        assert_eq!(
            source(true, false).http_failure(403, "host unreachable"),
            SourceError::Unavailable("ManagedIdentityCredential: unexpected response \"host unreachable\"".to_owned())
        );
        assert!(matches!(
            source(true, false).http_failure(403, "denied"),
            SourceError::Failed(_)
        ));
    }

    #[test]
    fn a_user_assigned_identity_that_is_not_assigned_stops_the_chain() {
        let SourceError::Failed(text) = source(true, true).http_failure(400, r#"{"error":"invalid_request"}"#) else {
            panic!("expected a failure");
        };

        assert!(text.starts_with("ManagedIdentityCredential authentication failed. the requested identity isn't assigned to this resource\nGET http://169.254.169.254/metadata/identity/oauth2/token\n"));
        assert!(text.contains("RESPONSE 400: 400 Bad Request"));
        assert!(text.ends_with("To troubleshoot, visit https://aka.ms/azsdk/go/identity/troubleshoot#managed-id"));
    }

    #[test]
    fn other_sources_never_treat_http_errors_as_unavailable() {
        assert!(matches!(
            source(false, false).http_failure(400, "x"),
            SourceError::Failed(_)
        ));
    }

    #[test]
    fn other_failures_stop_the_chain_except_unparsable_imds_replies() {
        let imds = source(true, false);
        let unparsable = azure_core::Error::new(ErrorKind::DataConversion, "bad json");
        let refused = azure_core::Error::new(ErrorKind::Io, "refused");

        assert!(matches!(imds.classify(&unparsable), SourceError::Unavailable(_)));
        assert!(matches!(imds.classify(&refused), SourceError::Failed(_)));
        assert!(matches!(
            source(false, false).classify(&unparsable),
            SourceError::Failed(_)
        ));
    }
}
