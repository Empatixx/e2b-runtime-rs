use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::Client;

use super::aad::Authority;
use super::token_source::{BearerToken, SourceError, TokenSource};
use super::ws_trust;

const CREDENTIAL: &str = "UsernamePasswordCredential";

pub struct UsernamePasswordCredential {
    pub client_id: String,
    pub username: String,
    pub password: String,
    pub authority: Authority,
    pub client: Client,
}

impl UsernamePasswordCredential {
    fn failed(message: String) -> SourceError {
        SourceError::Failed(format!("{CREDENTIAL}: {message}"))
    }

    fn form(&self, grant_type: &str) -> Vec<(&'static str, String)> {
        vec![
            ("grant_type", grant_type.to_owned()),
            ("username", self.username.clone()),
            ("password", self.password.clone()),
            ("client_id", self.client_id.clone()),
            ("client_info", "1".to_owned()),
        ]
    }

    async fn managed(&self, authority: &Authority, scope: &str) -> Result<BearerToken, SourceError> {
        authority
            .request_token(&self.client, CREDENTIAL, scope, self.form("password"))
            .await
    }

    async fn federated(&self, realm: &ws_trust::UserRealm, scope: &str) -> Result<BearerToken, SourceError> {
        let metadata_url = &realm.federation_metadata_url;
        let endpoint = ws_trust::mex(&self.client, metadata_url).await.map_err(|e| {
            e.failed(CREDENTIAL, |message| {
                format!("problem getting mex doc from federated url({metadata_url}): {message}")
            })
        })?;
        let saml = ws_trust::saml_token(
            &self.client,
            &endpoint,
            &realm.cloud_audience_urn,
            &self.username,
            &self.password,
        )
        .await
        .map_err(|e| {
            e.failed(CREDENTIAL, |message| {
                format!("problem getting SAML token info: {message}")
            })
        })?;
        let mut form = self.form(saml.grant_type);
        form.push(("assertion", STANDARD.encode(saml.assertion)));
        self.authority
            .request_token(&self.client, CREDENTIAL, scope, form)
            .await
    }
}

#[async_trait::async_trait]
impl TokenSource for UsernamePasswordCredential {
    async fn token(&self, scope: &str) -> Result<BearerToken, SourceError> {
        if self.authority.is_adfs() {
            let resolved = self
                .authority
                .resolved(&self.client)
                .await
                .map_err(|e| e.failed(CREDENTIAL, |message| message))?;
            return self.managed(&resolved, scope).await;
        }
        let realm = ws_trust::user_realm(&self.client, self.authority.base_url(), &self.username)
            .await
            .map_err(|e| {
                e.failed(CREDENTIAL, |message| {
                    format!("problem getting user realm from authority: {message}")
                })
            })?;
        match realm.account_type.as_str() {
            "Federated" => self.federated(&realm, scope).await,
            "Managed" => self.managed(&self.authority, scope).await,
            _ => Err(Self::failed("unknown account type".to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::serve;
    use super::*;

    const SEPARATOR: &str = "--------------------------------------------------------------------------------";

    fn credential(base_url: &str) -> UsernamePasswordCredential {
        UsernamePasswordCredential {
            client_id: "client".to_owned(),
            username: "me@example.com".to_owned(),
            password: "pw".to_owned(),
            authority: Authority::local(base_url),
            client: Client::new(),
        }
    }

    fn realm(account_type: &str, extra: &str) -> String {
        format!(
            r#"{{"account_type":"{account_type}","domain_name":"example.com","cloud_instance_name":"microsoftonline.com","cloud_audience_urn":"urn:federation:MicrosoftOnline"{extra}}}"#
        )
    }

    #[tokio::test]
    async fn managed_users_use_the_password_grant() {
        let (base, handle) = serve(vec![
            (200, "application/json", realm("Managed", "")),
            (
                200,
                "application/json",
                r#"{"access_token":"aad","expires_in":3600}"#.to_owned(),
            ),
        ])
        .await;

        let token = credential(&base).token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[0].starts_with("GET /common/UserRealm/me@example.com?api-version=1.0 HTTP/1.1"));
        assert!(requests[1].contains("grant_type=password"));
        assert!(requests[1].contains("username=me%40example.com"));
    }

    #[tokio::test]
    async fn managed_rejections_read_like_the_sdk() {
        let (base, _handle) = serve(vec![
            (200, "application/json", realm("Managed", "")),
            (400, "application/json", r#"{"error":"invalid_grant"}"#.to_owned()),
        ])
        .await;

        let error = credential(&base).token("s").await.unwrap_err();

        assert!(
            matches!(&error, SourceError::Failed(text) if text.starts_with("UsernamePasswordCredential authentication failed. \nPOST http://") && text.contains("RESPONSE 400: 400 Bad Request"))
        );
    }

    const MEX: &str = r##"<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:wsu="u" xmlns:sp="s" xmlns:wsp="p" xmlns:soap12="s12">
<wsp:Policy wsu:Id="pol"><wsp:ExactlyOne><wsp:All><sp:TransportBinding xmlns:sp="sp"/><sp:SignedSupportingTokens><wsp:Policy><sp:UsernameToken><wsp:Policy><sp:WssUsernameToken10/></wsp:Policy></sp:UsernameToken></wsp:Policy></sp:SignedSupportingTokens></wsp:All></wsp:ExactlyOne></wsp:Policy>
<binding name="b13"><wsp:PolicyReference URI="#pol"/><soap12:binding transport="http://schemas.xmlsoap.org/soap/http"/><operation name="Issue"><soap12:operation soapAction="http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue"/></operation></binding>
<service name="S"><port name="p" binding="tns:b13"><wsa:EndpointReference xmlns:wsa="a"><wsa:Address>{BASE}/trust</wsa:Address></wsa:EndpointReference></port></service>
</definitions>"##;

    const SAML: &str = r#"<s:Envelope xmlns:s="e"><s:Body><t:RequestSecurityTokenResponseCollection xmlns:t="t"><t:RequestSecurityTokenResponse><t:RequestedSecurityToken><saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion"><x/></saml:Assertion></t:RequestedSecurityToken></t:RequestSecurityTokenResponse></t:RequestSecurityTokenResponseCollection></s:Body></s:Envelope>"#;

    #[tokio::test]
    async fn federated_users_trade_a_saml_assertion_from_the_ws_trust_endpoint() {
        let (base, handle) = serve(vec![
            (
                200,
                "application/json",
                realm(
                    "Federated",
                    r#","federation_protocol":"WSTrust","federation_metadata_url":"{BASE}/mex""#,
                ),
            ),
            (200, "application/xml", MEX.to_owned()),
            (200, "application/soap+xml", SAML.to_owned()),
            (
                200,
                "application/json",
                r#"{"access_token":"aad","expires_in":3600}"#.to_owned(),
            ),
        ])
        .await;

        let token = credential(&base).token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[1].starts_with("GET /mex HTTP/1.1"));
        assert!(requests[2].starts_with("POST /trust HTTP/1.1"));
        assert!(requests[2].contains("soapaction: http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue"));
        assert!(requests[2].contains("content-type: application/soap+xml; charset=utf-8"));
        assert!(requests[2].contains("<wsse:Username>me@example.com</wsse:Username>"));
        assert!(requests[3].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Asaml2-bearer"));
        let assertion = STANDARD
            .encode(r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion"><x/></saml:Assertion>"#);
        assert!(requests[3].contains(&format!("assertion={}", assertion.replace('=', "%3D"))));
    }

    #[tokio::test]
    async fn federation_failures_name_the_failing_step() {
        let (base, _handle) = serve(vec![
            (
                200,
                "application/json",
                realm(
                    "Federated",
                    r#","federation_protocol":"WSTrust","federation_metadata_url":"{BASE}/mex""#,
                ),
            ),
            (404, "text/plain", "gone".to_owned()),
        ])
        .await;

        let error = credential(&base).token("s").await.unwrap_err();

        assert_eq!(
            error,
            SourceError::Failed(format!(
                "UsernamePasswordCredential authentication failed. \nGET {base}/mex\n{SEPARATOR}\nRESPONSE 404: 404 Not Found\n{SEPARATOR}\ngone\n{SEPARATOR}\n"
            ))
        );
    }

    #[tokio::test]
    async fn a_missing_user_realm_reads_like_the_sdk() {
        let (base, _handle) = serve(vec![(404, "application/json", r#"{"error":"nope"}"#.to_owned())]).await;

        let error = credential(&base).token("s").await.unwrap_err();

        assert_eq!(
            error,
            SourceError::Failed(format!(
                "UsernamePasswordCredential authentication failed. \nGET {base}/common/UserRealm/me@example.com\n{SEPARATOR}\nRESPONSE 404: 404 Not Found\n{SEPARATOR}\n{{\n  \"error\": \"nope\"\n}}\n{SEPARATOR}\n"
            ))
        );
    }

    #[tokio::test]
    async fn adfs_authorities_discover_the_token_endpoint_and_skip_the_user_realm() {
        let (base, handle) = serve(vec![
            (
                200,
                "application/json",
                r#"{"authorization_endpoint":"{BASE}/adfs/oauth2/authorize","token_endpoint":"{BASE}/adfs/oauth2/token","issuer":"{BASE}/adfs"}"#.to_owned(),
            ),
            (200, "application/json", r#"{"access_token":"aad","expires_in":3600}"#.to_owned()),
        ])
        .await;
        let mut credential = credential(&base);
        credential.authority = Authority::local_adfs(&base);

        let token = credential.token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[0].starts_with("GET /adfs/.well-known/openid-configuration HTTP/1.1"));
        assert!(requests[1].starts_with("POST /adfs/oauth2/token HTTP/1.1"));
        assert!(requests[1].contains("grant_type=password"));
    }

    #[tokio::test]
    async fn adfs_discovery_runs_once_per_credential() {
        let (base, handle) = serve(vec![
            (
                200,
                "application/json",
                r#"{"authorization_endpoint":"{BASE}/adfs/oauth2/authorize","token_endpoint":"{BASE}/adfs/oauth2/token","issuer":"{BASE}/adfs"}"#.to_owned(),
            ),
            (200, "application/json", r#"{"access_token":"one","expires_in":3600}"#.to_owned()),
            (200, "application/json", r#"{"access_token":"two","expires_in":3600}"#.to_owned()),
        ])
        .await;
        let mut credential = credential(&base);
        credential.authority = Authority::local_adfs(&base);

        credential.token("s").await.unwrap();
        credential.token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /adfs/.well-known/openid-configuration HTTP/1.1"));
        assert!(requests[2].starts_with("POST /adfs/oauth2/token HTTP/1.1"));
    }

    #[tokio::test]
    async fn incomplete_adfs_discovery_documents_are_rejected() {
        let (base, _handle) = serve(vec![(200, "application/json", r#"{"issuer":"x"}"#.to_owned())]).await;
        let mut credential = credential(&base);
        credential.authority = Authority::local_adfs(&base);

        assert_eq!(
            credential.token("s").await.unwrap_err(),
            SourceError::Failed("UsernamePasswordCredential: unable to resolve an endpoint: ResolveEndpoints(): TenantDiscoveryResponse: authorize endpoint was not found in the openid configuration".to_owned())
        );
    }
}
