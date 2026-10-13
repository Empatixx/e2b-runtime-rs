use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;

use super::azcore::{CallError, HttpFailure};
use super::xml::{self, Element};

const TRUST_13_SPEC: &str = "http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue";
const TRUST_2005_SPEC: &str = "http://schemas.xmlsoap.org/ws/2005/02/trust/RST/Issue";
const SOAP_HTTP: &str = "http://schemas.xmlsoap.org/soap/http";
const SAML_V1: &str = "urn:oasis:names:tc:SAML:1.0:assertion";
const SAML_V2: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
pub const SAML_V1_GRANT: &str = "urn:ietf:params:oauth:grant-type:saml1_1-bearer";
pub const SAML_V2_GRANT: &str = "urn:ietf:params:oauth:grant-type:saml2-bearer";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Version {
    #[default]
    Unknown,
    Trust2005,
    Trust13,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub version: Version,
    pub url: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SamlToken {
    pub grant_type: &'static str,
    pub assertion: String,
}

impl std::fmt::Debug for SamlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamlToken")
            .field("grant_type", &self.grant_type)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct UserRealm {
    pub account_type: String,
    pub domain_name: String,
    pub cloud_instance_name: String,
    pub cloud_audience_urn: String,
    pub federation_protocol: String,
    pub federation_metadata_url: String,
}

impl UserRealm {
    pub fn validate(&self) -> Result<(), String> {
        let missing = [
            (&self.account_type, "the account type (Federated or Managed) is missing"),
            (&self.domain_name, "domain name of user realm is missing"),
            (
                &self.cloud_instance_name,
                "cloud instance name of user realm is missing",
            ),
            (&self.cloud_audience_urn, "cloud Instance URN is missing"),
        ]
        .into_iter()
        .find(|(value, _)| value.is_empty());
        if let Some((_, message)) = missing {
            return Err(message.to_owned());
        }
        match (
            self.account_type == "Federated",
            &self.federation_protocol,
            &self.federation_metadata_url,
        ) {
            (true, protocol, _) if protocol.is_empty() => {
                Err("federation protocol of user realm is missing".to_owned())
            }
            (true, _, url) if url.is_empty() => Err("federation metadata URL of user realm is missing".to_owned()),
            _ => Ok(()),
        }
    }
}

async fn read(response: reqwest::Response, method: &str, url: &str) -> Result<String, CallError> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| CallError::Message(format!("could not read the body of an HTTP Response: {e}")))?;
    match status.as_u16() {
        200 | 201 => Ok(body),
        _ => Err(CallError::Http(HttpFailure {
            method: method.to_owned(),
            url: url.to_owned(),
            status,
            body,
        })),
    }
}

fn transport_error(error: reqwest::Error) -> CallError {
    CallError::Message(format!("server response error:\n {error}"))
}

pub async fn user_realm(client: &Client, base_url: &str, username: &str) -> Result<UserRealm, CallError> {
    let url = format!(
        "{base_url}/common/UserRealm/{}?api-version=1.0",
        super::path_escape(username)
    );
    let response = client
        .get(&url)
        .header("client-request-id", uuid::Uuid::new_v4().to_string())
        .send()
        .await
        .map_err(transport_error)?;
    let body = read(response, "GET", &url).await?;
    let realm: UserRealm = serde_json::from_str(&body)
        .map_err(|e| CallError::Message(format!("json decode error: {e}\njson message bytes were: {body}")))?;
    realm.validate().map(|()| realm).map_err(CallError::Message)
}

pub async fn mex(client: &Client, url: &str) -> Result<Endpoint, CallError> {
    let response = client
        .get(url)
        .header("Content-Type", "application/xml; charset=utf-8")
        .send()
        .await
        .map_err(transport_error)?;
    let body = read(response, "GET", url).await?;
    xml::parse(&body)
        .map_err(|e| format!("XML syntax error: {e}"))
        .and_then(|definitions| username_password_endpoint(&definitions))
        .map_err(CallError::Message)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointType {
    UsernamePassword,
    WindowsTransport,
}

struct Policy {
    id: String,
    kind: EndpointType,
}

struct Binding {
    name: String,
    version: Version,
    kind: EndpointType,
}

struct Located {
    kind: EndpointType,
    endpoint: Endpoint,
}

#[derive(Default)]
struct Endpoints {
    username_password: Endpoint,
    windows: Endpoint,
}

impl Endpoint {
    fn replaced_by(&self, found: &Endpoint) -> bool {
        self.version == Version::Unknown || (self.version == Version::Trust2005 && found.version == Version::Trust13)
    }
}

impl Endpoints {
    fn offer(mut self, found: Located) -> Self {
        let slot = match found.kind {
            EndpointType::UsernamePassword => &mut self.username_password,
            EndpointType::WindowsTransport => &mut self.windows,
        };
        if slot.replaced_by(&found.endpoint) {
            *slot = found.endpoint;
        }
        self
    }
}

fn policies(definitions: &Element) -> Result<Vec<Policy>, String> {
    let found: Vec<Policy> = definitions.children_named("Policy").flat_map(classify).collect();
    match found.is_empty() {
        true => Err("no policies for mex document".to_owned()),
        false => Ok(found),
    }
}

fn classify(policy: &Element) -> Vec<Policy> {
    let id = policy.attribute("Id").unwrap_or_default();
    let Some(all) = policy.path(&["ExactlyOne", "All"]) else {
        return Vec::new();
    };
    let transport = all
        .child("TransportBinding")
        .is_some_and(|binding| binding.attribute("sp").is_some_and(|sp| !sp.is_empty()));
    let usable = transport && !id.is_empty();
    let username_token = ["SignedEncryptedSupportingTokens", "SignedSupportingTokens"]
        .iter()
        .any(|name| {
            all.path(&[name, "Policy", "UsernameToken", "Policy", "WssUsernameToken10"])
                .is_some()
        });
    [
        (
            all.child("NegotiateAuthentication").is_some(),
            EndpointType::WindowsTransport,
        ),
        (username_token, EndpointType::UsernamePassword),
    ]
    .into_iter()
    .filter(|(present, _)| *present && usable)
    .map(|(_, kind)| Policy {
        id: format!("#{id}"),
        kind,
    })
    .collect()
}

fn soap_version(action: &str) -> Result<Version, String> {
    match action {
        TRUST_13_SPEC => Ok(Version::Trust13),
        TRUST_2005_SPEC => Ok(Version::Trust2005),
        _ => Err("found unknown spec version in mex document".to_owned()),
    }
}

fn binding_of(binding: &Element, policies: &[Policy]) -> Result<Option<Binding>, String> {
    let reference = binding
        .child("PolicyReference")
        .and_then(|r| r.attribute("URI"))
        .unwrap_or_default();
    let transport = binding
        .child("binding")
        .and_then(|b| b.attribute("transport"))
        .unwrap_or_default();
    let Some(policy) = policies.iter().rev().find(|policy| policy.id == reference) else {
        return Ok(None);
    };
    if transport != SOAP_HTTP {
        return Ok(None);
    }
    let action = binding
        .path(&["operation", "operation"])
        .and_then(|operation| operation.attribute("soapAction"))
        .unwrap_or_default();
    soap_version(action).map(|version| {
        Some(Binding {
            name: binding.attribute("name").unwrap_or_default().to_owned(),
            version,
            kind: policy.kind,
        })
    })
}

fn bindings(definitions: &Element, policies: &[Policy]) -> Result<Vec<Binding>, String> {
    definitions
        .children_named("binding")
        .filter_map(|binding| binding_of(binding, policies).transpose())
        .collect()
}

fn located(port: &Element, bindings: &[Binding]) -> Result<Option<Located>, String> {
    let reference = port.attribute("binding").unwrap_or_default();
    let name = reference.split_once(':').map_or(reference, |(_, rest)| rest);
    let Some(binding) = bindings.iter().find(|binding| binding.name == name) else {
        return Ok(None);
    };
    let url = port
        .path(&["EndpointReference", "Address"])
        .map(|address| address.text.trim().to_owned())
        .unwrap_or_default();
    match url.is_empty() {
        true => Err("MexDocument cannot have blank URL endpoint".to_owned()),
        false => Ok(Some(Located {
            kind: binding.kind,
            endpoint: Endpoint {
                version: binding.version,
                url,
            },
        })),
    }
}

fn endpoints(definitions: &Element, bindings: &[Binding]) -> Result<Endpoints, String> {
    definitions
        .child("service")
        .into_iter()
        .flat_map(|service| service.children_named("port"))
        .try_fold(Endpoints::default(), |found, port| {
            located(port, bindings).map(|located| match located {
                Some(located) => found.offer(located),
                None => found,
            })
        })
}

fn username_password_endpoint(definitions: &Element) -> Result<Endpoint, String> {
    let policies = policies(definitions)?;
    let bindings = bindings(definitions, &policies)?;
    endpoints(definitions, &bindings).map(|found| found.username_password)
}

fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '"' => "&#34;".to_owned(),
            '\'' => "&#39;".to_owned(),
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '\t' => "&#x9;".to_owned(),
            '\n' => "&#xA;".to_owned(),
            '\r' => "&#xD;".to_owned(),
            c => c.to_string(),
        })
        .collect()
}

fn time_string(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

struct TrustSpec {
    action: &'static str,
    namespace: &'static str,
    key_type: &'static str,
    request_type: &'static str,
}

impl TrustSpec {
    fn of(version: Version) -> Option<Self> {
        match version {
            Version::Trust2005 => Some(Self {
                action: TRUST_2005_SPEC,
                namespace: "http://schemas.xmlsoap.org/ws/2005/02/trust",
                key_type: "http://schemas.xmlsoap.org/ws/2005/05/identity/NoProofKey",
                request_type: "http://schemas.xmlsoap.org/ws/2005/02/trust/Issue",
            }),
            Version::Trust13 => Some(Self {
                action: TRUST_13_SPEC,
                namespace: "http://docs.oasis-open.org/ws-sx/ws-trust/200512",
                key_type: "http://docs.oasis-open.org/ws-sx/ws-trust/200512/Bearer",
                request_type: "http://docs.oasis-open.org/ws-sx/ws-trust/200512/Issue",
            }),
            Version::Unknown => None,
        }
    }
}

pub fn token_request(
    endpoint: &Endpoint,
    audience: &str,
    username: &str,
    password: &str,
    now: DateTime<Utc>,
) -> Result<String, String> {
    let spec = TrustSpec::of(endpoint.version).ok_or_else(|| {
        "buildTokenRequestMessage had Version == \"TrustUnknown\", which is not recognized".to_owned()
    })?;
    let token_id = match endpoint.version {
        Version::Trust2005 => format!("UnPwSecTok2005-{}", uuid::Uuid::new_v4()),
        _ => format!("UnPwSecTok13-{}", uuid::Uuid::new_v4()),
    };
    Ok(format!(
        concat!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">"#,
            r#"<s:Header><wsa:Action s:mustUnderstand="1">{action}</wsa:Action><wsa:messageID>urn:uuid:{message_id}</wsa:messageID>"#,
            r#"<wsa:ReplyTo><wsa:Address>http://www.w3.org/2005/08/addressing/anonymous</wsa:Address></wsa:ReplyTo>"#,
            r#"<wsa:To s:mustUnderstand="1">{url}</wsa:To>"#,
            r#"<wsse:Security s:mustUnderstand="1" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd">"#,
            r#"<wsu:Timestamp wsu:Id="MSATimeStamp"><wsu:Created>{created}</wsu:Created><wsu:Expires>{expires}</wsu:Expires></wsu:Timestamp>"#,
            r#"<wsse:UsernameToken wsu:Id="{token_id}"><wsse:Username>{username}</wsse:Username><wsse:Password>{password}</wsse:Password></wsse:UsernameToken>"#,
            r#"</wsse:Security></s:Header>"#,
            r#"<s:Body><wst:RequestSecurityToken xmlns:wst="{namespace}"><wsp:AppliesTo xmlns:wsp="http://schemas.xmlsoap.org/ws/2004/09/policy">"#,
            r#"<wsa:EndpointReference><wsa:Address>{audience}</wsa:Address></wsa:EndpointReference></wsp:AppliesTo>"#,
            r#"<wst:KeyType>{key_type}</wst:KeyType><wst:RequestType>{request_type}</wst:RequestType></wst:RequestSecurityToken></s:Body></s:Envelope>"#
        ),
        action = spec.action,
        message_id = uuid::Uuid::new_v4(),
        url = escape(&endpoint.url),
        created = time_string(now),
        expires = time_string(now + Duration::minutes(10)),
        token_id = token_id,
        username = escape(username),
        password = escape(password),
        namespace = spec.namespace,
        audience = escape(audience),
        key_type = spec.key_type,
        request_type = spec.request_type,
    ))
}

pub async fn saml_token(
    client: &Client,
    endpoint: &Endpoint,
    audience: &str,
    username: &str,
    password: &str,
) -> Result<SamlToken, CallError> {
    let message = token_request(endpoint, audience, username, password, Utc::now()).map_err(CallError::Message)?;
    if endpoint.version == Version::Trust2005 {
        return Err(CallError::Message(
            "WS Trust 2005 support is not implemented".to_owned(),
        ));
    }
    let response = client
        .post(&endpoint.url)
        .header("Content-Type", "application/soap+xml; charset=utf-8")
        .header("SOAPAction", TRUST_13_SPEC)
        .body(message)
        .send()
        .await
        .map_err(transport_error)?;
    let body = read(response, "POST", &endpoint.url).await?;
    assertion(&body).map_err(CallError::Message)
}

pub fn assertion(body: &str) -> Result<SamlToken, String> {
    let envelope = xml::parse(body).map_err(|e| format!("XML syntax error: {e}"))?;
    let collection = envelope.path(&["Body", "RequestSecurityTokenResponseCollection"]);
    for response in collection
        .into_iter()
        .flat_map(|c| c.children_named("RequestSecurityTokenResponse"))
    {
        let Some(token) = response.child("RequestedSecurityToken") else {
            continue;
        };
        let Some(assertion) = token.child("Assertion") else {
            continue;
        };
        let raw = body[token.inner.start..token.inner.end].to_owned();
        let version = assertion.attribute("saml").unwrap_or_default();
        return match version {
            SAML_V1 => Ok(SamlToken {
                grant_type: SAML_V1_GRANT,
                assertion: raw,
            }),
            SAML_V2 => Ok(SamlToken {
                grant_type: SAML_V2_GRANT,
                assertion: raw,
            }),
            other => Err(format!("couldn't parse SAML assertion, version unknown: {other:?}")),
        };
    }
    Err("unknown WS-Trust version".to_owned())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    const MEX: &str = r##"<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:wsu="u" xmlns:sp="http://docs.oasis-open.org/ws-sx/ws-securitypolicy/200702" xmlns:wsp="p" xmlns:soap12="s12">
<wsp:Policy wsu:Id="UserNameWSTrustBinding_IWSTrust13Async_policy"><wsp:ExactlyOne><wsp:All>
<sp:TransportBinding xmlns:sp="sp"><wsp:Policy/></sp:TransportBinding>
<sp:SignedSupportingTokens><wsp:Policy><sp:UsernameToken><wsp:Policy><sp:WssUsernameToken10/></wsp:Policy></sp:UsernameToken></wsp:Policy></sp:SignedSupportingTokens>
</wsp:All></wsp:ExactlyOne></wsp:Policy>
<binding name="UserNameWSTrustBinding_IWSTrust13Async" type="tns:IWSTrust13Async"><wsp:PolicyReference URI="#UserNameWSTrustBinding_IWSTrust13Async_policy"/><soap12:binding transport="http://schemas.xmlsoap.org/soap/http"/><operation name="Issue"><soap12:operation soapAction="http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue" style="document"/></operation></binding>
<service name="SecurityTokenService"><port name="p1" binding="tns:UserNameWSTrustBinding_IWSTrust13Async"><soap12:address location="x"/><wsa:EndpointReference xmlns:wsa="a"><wsa:Address>https://adfs.example.com/adfs/services/trust/13/usernamemixed</wsa:Address></wsa:EndpointReference></port></service>
</definitions>"##;

    #[test]
    fn the_mex_document_yields_the_username_password_endpoint() {
        let definitions = xml::parse(MEX).unwrap();

        assert_eq!(
            username_password_endpoint(&definitions).unwrap(),
            Endpoint {
                version: Version::Trust13,
                url: "https://adfs.example.com/adfs/services/trust/13/usernamemixed".to_owned()
            }
        );
        assert_eq!(
            username_password_endpoint(&xml::parse("<definitions/>").unwrap()).unwrap_err(),
            "no policies for mex document"
        );
    }

    #[test]
    fn the_token_request_is_the_marshalled_envelope() {
        let endpoint = Endpoint {
            version: Version::Trust13,
            url: "https://adfs/x".to_owned(),
        };
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();

        let message = token_request(&endpoint, "urn:federation:MicrosoftOnline", "u&ser", "p<w", now).unwrap();

        assert!(message.starts_with(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:wsu=""#));
        assert!(message.contains(r#"<wsa:To s:mustUnderstand="1">https://adfs/x</wsa:To>"#));
        assert!(message.contains(
            "<wsu:Created>2026-01-02T03:04:05.000Z</wsu:Created><wsu:Expires>2026-01-02T03:14:05.000Z</wsu:Expires>"
        ));
        assert!(message.contains("<wsse:Username>u&amp;ser</wsse:Username><wsse:Password>p&lt;w</wsse:Password>"));
        assert!(message.contains("<wst:KeyType>http://docs.oasis-open.org/ws-sx/ws-trust/200512/Bearer</wst:KeyType>"));
        assert!(message.ends_with("</wst:RequestSecurityToken></s:Body></s:Envelope>"));
        assert_eq!(
            token_request(&Endpoint::default(), "a", "u", "p", now).unwrap_err(),
            "buildTokenRequestMessage had Version == \"TrustUnknown\", which is not recognized"
        );
    }

    #[test]
    fn the_saml_assertion_is_the_raw_security_token_xml() {
        let response = r#"<s:Envelope xmlns:s="e"><s:Body><wst:RequestSecurityTokenResponseCollection xmlns:wst="t"><wst:RequestSecurityTokenResponse><wst:RequestedSecurityToken><saml:Assertion MajorVersion="1" xmlns:saml="urn:oasis:names:tc:SAML:1.0:assertion"><x/></saml:Assertion></wst:RequestedSecurityToken></wst:RequestSecurityTokenResponse></wst:RequestSecurityTokenResponseCollection></s:Body></s:Envelope>"#;

        let token = assertion(response).unwrap();

        assert_eq!(token.grant_type, SAML_V1_GRANT);
        assert_eq!(
            token.assertion,
            r#"<saml:Assertion MajorVersion="1" xmlns:saml="urn:oasis:names:tc:SAML:1.0:assertion"><x/></saml:Assertion>"#
        );
        assert_eq!(assertion("<Envelope/>").unwrap_err(), "unknown WS-Trust version");
    }

    #[test]
    fn realms_are_validated_with_the_sdk_texts() {
        let realm: UserRealm = serde_json::from_str(
            r#"{"account_type":"Federated","domain_name":"d","cloud_instance_name":"c","cloud_audience_urn":"u"}"#,
        )
        .unwrap();

        assert_eq!(
            realm.validate().unwrap_err(),
            "federation protocol of user realm is missing"
        );
        assert_eq!(
            UserRealm::default().validate().unwrap_err(),
            "the account type (Federated or Managed) is missing"
        );
    }
}
