use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::Client;
use tokio::sync::Mutex;

use super::aad::{Authority, Exchange, Grant};
use super::msal::{CacheKey, TokenCache};
use super::tenant::TenantPolicy;
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource};
use super::ws_trust;

const CREDENTIAL: &str = "UsernamePasswordCredential";

pub struct UsernamePasswordCredential {
    pub client_id: String,
    pub username: String,
    pub password: String,
    pub authority: Authority,
    pub tenants: TenantPolicy,
    pub client: Client,
    pub cache: Mutex<TokenCache>,
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

    fn exchange<'a>(request: &'a TokenRequest<'_>, form: Vec<(&'static str, String)>) -> Exchange<'a> {
        Exchange {
            credential: CREDENTIAL,
            scope: request.scope,
            cae: request.cae,
            form,
        }
    }

    async fn refreshed(
        &self,
        authority: &Authority,
        request: &TokenRequest<'_>,
        refresh_token: String,
    ) -> Result<Grant, SourceError> {
        let endpoints = authority
            .resolve(&self.client)
            .await
            .map_err(|e| e.failed(CREDENTIAL, |message| message))?;
        let form = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("client_id", self.client_id.clone()),
            ("client_info", "1".to_owned()),
            ("refresh_token", refresh_token),
        ];
        endpoints.exchange(&self.client, Self::exchange(request, form)).await
    }

    async fn silent(
        &self,
        cache: &mut TokenCache,
        authority: &Authority,
        request: &TokenRequest<'_>,
        key: &CacheKey,
    ) -> Option<BearerToken> {
        let cached = cache.valid(key).filter(|token| !token.refresh_due());
        if cached.is_some() {
            return cached;
        }
        let refresh_token = cache.refresh_token(request.cae)?;
        let grant = self.refreshed(authority, request, refresh_token).await.ok()?;
        cache.store(key.clone(), &grant);
        Some(grant.token)
    }

    async fn authenticate(&self, authority: &Authority, request: &TokenRequest<'_>) -> Result<Grant, SourceError> {
        let endpoints = authority
            .resolve(&self.client)
            .await
            .map_err(|e| e.failed(CREDENTIAL, |message| message))?;
        if authority.is_adfs() {
            return endpoints
                .exchange(&self.client, Self::exchange(request, self.form("password")))
                .await;
        }
        let realm = ws_trust::user_realm(&self.client, &endpoints.origin, &self.username)
            .await
            .map_err(|e| {
                e.failed(CREDENTIAL, |message| {
                    format!("problem getting user realm from authority: {message}")
                })
            })?;
        match realm.account_type.as_str() {
            "Federated" => self.federated(&endpoints, &realm, request).await,
            "Managed" => {
                endpoints
                    .exchange(&self.client, Self::exchange(request, self.form("password")))
                    .await
            }
            _ => Err(Self::failed("unknown account type".to_owned())),
        }
    }

    async fn federated(
        &self,
        endpoints: &super::aad::Endpoints,
        realm: &ws_trust::UserRealm,
        request: &TokenRequest<'_>,
    ) -> Result<Grant, SourceError> {
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
        endpoints.exchange(&self.client, Self::exchange(request, form)).await
    }
}

#[async_trait::async_trait]
impl TokenSource for UsernamePasswordCredential {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let authority = self.authority.for_request(&self.tenants, CREDENTIAL, request.tenant)?;
        let key = CacheKey::of(request, authority.tenant());
        let mut cache = self.cache.lock().await;
        if let Some(token) = self.silent(&mut cache, &authority, request, &key).await {
            return Ok(token);
        }
        let grant = self.authenticate(&authority, request).await?;
        cache.store(key, &grant);
        Ok(grant.token)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::serve;
    use super::*;

    const SEPARATOR: &str = "--------------------------------------------------------------------------------";

    fn credential(authority: Authority, default_tenant: &str) -> UsernamePasswordCredential {
        UsernamePasswordCredential {
            client_id: "client".to_owned(),
            username: "me@example.com".to_owned(),
            password: "pw".to_owned(),
            authority,
            tenants: TenantPolicy::new(default_tenant, &[]),
            client: Client::new(),
            cache: Mutex::default(),
        }
    }

    fn aad(base_url: &str) -> UsernamePasswordCredential {
        credential(Authority::local(base_url), "t")
    }

    fn adfs(base_url: &str) -> UsernamePasswordCredential {
        credential(Authority::local_adfs(base_url), "adfs")
    }

    fn configuration() -> (u16, &'static str, String) {
        (
            200,
            "application/json",
            r#"{"authorization_endpoint":"{BASE}/t/authorize","token_endpoint":"{BASE}/t/oauth2/v2.0/token","issuer":"{BASE}/t/v2.0"}"#.to_owned(),
        )
    }

    fn adfs_configuration() -> (u16, &'static str, String) {
        (
            200,
            "application/json",
            r#"{"authorization_endpoint":"{BASE}/adfs/oauth2/authorize","token_endpoint":"{BASE}/adfs/oauth2/token","issuer":"{BASE}/adfs"}"#.to_owned(),
        )
    }

    fn token_reply(extra: &str) -> (u16, &'static str, String) {
        (
            200,
            "application/json",
            format!(r#"{{"access_token":"aad","expires_in":3600{extra}}}"#),
        )
    }

    fn realm(account_type: &str, extra: &str) -> String {
        format!(
            r#"{{"account_type":"{account_type}","domain_name":"example.com","cloud_instance_name":"microsoftonline.com","cloud_audience_urn":"urn:federation:MicrosoftOnline"{extra}}}"#
        )
    }

    #[tokio::test]
    async fn managed_users_use_the_password_grant() {
        let (base, handle) = serve(vec![
            configuration(),
            (200, "application/json", realm("Managed", "")),
            token_reply(""),
        ])
        .await;

        let token = aad(&base).token_for(&TokenRequest::new("s").with_cae()).await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[0].starts_with("GET /t/v2.0/.well-known/openid-configuration HTTP/1.1"));
        assert!(requests[1].starts_with("GET /common/UserRealm/me@example.com?api-version=1.0 HTTP/1.1"));
        assert!(requests[2].starts_with("POST /t/oauth2/v2.0/token HTTP/1.1"));
        assert!(requests[2].contains("grant_type=password"));
        assert!(requests[2].contains("username=me%40example.com"));
        assert!(requests[2].contains("claims=%7B%22access_token%22"));
    }

    #[tokio::test]
    async fn the_cached_refresh_token_replaces_the_password_grant_once_the_access_token_is_stale() {
        let (base, handle) = serve(vec![
            configuration(),
            (200, "application/json", realm("Managed", "")),
            (
                200,
                "application/json",
                r#"{"access_token":"first","expires_in":10,"refresh_token":"rt-1"}"#.to_owned(),
            ),
            token_reply(r#","refresh_token":"rt-2""#),
        ])
        .await;
        let credential = aad(&base);

        let first = credential.token("s").await.unwrap();
        let second = credential.token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(first.secret, "first");
        assert_eq!(second.secret, "aad");
        assert_eq!(requests.len(), 4);
        assert!(requests[3].contains("grant_type=refresh_token"));
        assert!(requests[3].contains("refresh_token=rt-1"));
        assert!(!requests[3].contains("password=pw"));
    }

    #[tokio::test]
    async fn a_valid_cached_token_needs_no_request() {
        let (base, handle) = serve(vec![
            configuration(),
            (200, "application/json", realm("Managed", "")),
            token_reply(""),
        ])
        .await;
        let credential = aad(&base);

        credential.token("s").await.unwrap();
        credential.token("s").await.unwrap();

        assert_eq!(handle.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_failed_refresh_falls_back_to_the_password_grant() {
        let (base, handle) = serve(vec![
            configuration(),
            (200, "application/json", realm("Managed", "")),
            (
                200,
                "application/json",
                r#"{"access_token":"first","expires_in":10,"refresh_token":"rt-1"}"#.to_owned(),
            ),
            (400, "application/json", r#"{"error":"invalid_grant"}"#.to_owned()),
            (200, "application/json", realm("Managed", "")),
            token_reply(""),
        ])
        .await;
        let credential = aad(&base);

        credential.token("s").await.unwrap();
        let second = credential.token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(second.secret, "aad");
        assert!(requests[3].contains("grant_type=refresh_token"));
        assert!(requests[5].contains("grant_type=password"));
    }

    #[tokio::test]
    async fn managed_rejections_read_like_the_sdk() {
        let (base, _handle) = serve(vec![
            configuration(),
            (200, "application/json", realm("Managed", "")),
            (400, "application/json", r#"{"error":"invalid_grant"}"#.to_owned()),
        ])
        .await;

        let error = aad(&base).token("s").await.unwrap_err();

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

    fn federated_realm() -> (u16, &'static str, String) {
        (
            200,
            "application/json",
            realm(
                "Federated",
                r#","federation_protocol":"WSTrust","federation_metadata_url":"{BASE}/mex""#,
            ),
        )
    }

    #[tokio::test]
    async fn federated_users_trade_a_saml_assertion_from_the_ws_trust_endpoint() {
        let (base, handle) = serve(vec![
            configuration(),
            federated_realm(),
            (200, "application/xml", MEX.to_owned()),
            (200, "application/soap+xml", SAML.to_owned()),
            token_reply(""),
        ])
        .await;

        let token = aad(&base).token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[2].starts_with("GET /mex HTTP/1.1"));
        assert!(requests[3].starts_with("POST /trust HTTP/1.1"));
        assert!(requests[3].contains("soapaction: http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue"));
        assert!(requests[3].contains("content-type: application/soap+xml; charset=utf-8"));
        assert!(requests[3].contains("<wsse:Username>me@example.com</wsse:Username>"));
        assert!(requests[4].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Asaml2-bearer"));
        let assertion = STANDARD
            .encode(r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion"><x/></saml:Assertion>"#);
        assert!(requests[4].contains(&format!("assertion={}", assertion.replace('=', "%3D"))));
    }

    #[tokio::test]
    async fn federation_failures_name_the_failing_step() {
        let (base, _handle) = serve(vec![
            configuration(),
            federated_realm(),
            (404, "text/plain", "gone".to_owned()),
        ])
        .await;

        let error = aad(&base).token("s").await.unwrap_err();

        assert_eq!(
            error,
            SourceError::Failed(format!(
                "UsernamePasswordCredential authentication failed. \nGET {base}/mex\n{SEPARATOR}\nRESPONSE 404: 404 Not Found\n{SEPARATOR}\ngone\n{SEPARATOR}\n"
            ))
        );
    }

    #[tokio::test]
    async fn a_missing_user_realm_reads_like_the_sdk() {
        let (base, _handle) = serve(vec![
            configuration(),
            (404, "application/json", r#"{"error":"nope"}"#.to_owned()),
        ])
        .await;

        let error = aad(&base).token("s").await.unwrap_err();

        assert_eq!(
            error,
            SourceError::Failed(format!(
                "UsernamePasswordCredential authentication failed. \nGET {base}/common/UserRealm/me@example.com\n{SEPARATOR}\nRESPONSE 404: 404 Not Found\n{SEPARATOR}\n{{\n  \"error\": \"nope\"\n}}\n{SEPARATOR}\n"
            ))
        );
    }

    #[tokio::test]
    async fn adfs_authorities_discover_the_token_endpoint_and_skip_the_user_realm() {
        let (base, handle) = serve(vec![adfs_configuration(), token_reply("")]).await;

        let token = adfs(&base).token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[0].starts_with("GET /adfs/.well-known/openid-configuration HTTP/1.1"));
        assert!(requests[1].starts_with("POST /adfs/oauth2/token HTTP/1.1"));
        assert!(requests[1].contains("grant_type=password"));
    }

    #[tokio::test]
    async fn endpoint_discovery_runs_once_per_credential() {
        let (base, handle) = serve(vec![adfs_configuration(), token_reply(""), token_reply("")]).await;
        let credential = adfs(&base);

        credential.token("one").await.unwrap();
        credential.token("two").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /adfs/.well-known/openid-configuration HTTP/1.1"));
        assert!(requests[2].starts_with("POST /adfs/oauth2/token HTTP/1.1"));
    }

    #[tokio::test]
    async fn incomplete_adfs_discovery_documents_are_rejected() {
        let (base, _handle) = serve(vec![(200, "application/json", r#"{"issuer":"x"}"#.to_owned())]).await;

        assert_eq!(
            adfs(&base).token("s").await.unwrap_err(),
            SourceError::Failed("UsernamePasswordCredential: unable to resolve an endpoint: ResolveEndpoints(): TenantDiscoveryResponse: authorize endpoint was not found in the openid configuration".to_owned())
        );
    }

    #[tokio::test]
    async fn adfs_authorities_do_not_support_other_tenants() {
        let error = adfs("http://127.0.0.1:1")
            .token_for(&TokenRequest::new("s").with_tenant("other"))
            .await
            .unwrap_err();

        assert_eq!(error, SourceError::Failed("ADFS doesn't support tenants".to_owned()));
    }
}
