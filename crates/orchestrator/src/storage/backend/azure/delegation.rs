use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};

use super::azcore::{self, ResponseError};
use super::sas::{SERVICE_VERSION, format_time};

#[derive(Clone, PartialEq, Eq)]
pub struct UserDelegationKey {
    pub signed_oid: String,
    pub signed_tid: String,
    pub signed_start: DateTime<Utc>,
    pub signed_expiry: DateTime<Utc>,
    pub signed_service: String,
    pub signed_version: String,
    pub signed_delegated_user_tenant_id: String,
    pub value: Vec<u8>,
}

impl std::fmt::Debug for UserDelegationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserDelegationKey")
            .field("signed_oid", &self.signed_oid)
            .field("signed_tid", &self.signed_tid)
            .field("signed_expiry", &self.signed_expiry)
            .finish_non_exhaustive()
    }
}

pub struct DelegationRequest<'a> {
    pub service_url: &'a str,
    pub bearer: &'a str,
    pub start: DateTime<Utc>,
    pub expiry: DateTime<Utc>,
}

impl DelegationRequest<'_> {
    pub async fn fetch(&self, client: &Client) -> Result<UserDelegationKey, String> {
        let url = format!("{}?restype=service&comp=userdelegationkey", self.service_url);
        let body = format!(
            "<KeyInfo><Start>{}</Start><Expiry>{}</Expiry></KeyInfo>",
            format_time(self.start),
            format_time(self.expiry)
        );
        let request = client
            .post(&url)
            .bearer_auth(self.bearer)
            .header("x-ms-version", SERVICE_VERSION)
            .header("Accept", "application/xml")
            .header("Content-Type", "application/xml")
            .body(body);
        let response = azcore::send(request).await?;
        let status = response.status();
        let error_code = response
            .headers()
            .get("x-ms-error-code")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response.text().await.map_err(|e| e.to_string())?;
        match status {
            StatusCode::OK => UserDelegationKey::from_xml(&text),
            _ => Err(ResponseError {
                method: "POST",
                url: &url,
                status,
                error_code,
                body: &text,
            }
            .text()),
        }
    }
}

impl UserDelegationKey {
    pub fn from_xml(xml: &str) -> Result<Self, String> {
        let field = |name: &str| tag(xml, name).ok_or_else(|| format!("missing {name} in the user delegation key"));
        let time = |name: &str| {
            field(name).and_then(|text| {
                DateTime::parse_from_rfc3339(&text)
                    .map(|time| time.with_timezone(&Utc))
                    .map_err(|e| format!("parsing time {text:?}: {e}"))
            })
        };
        Ok(Self {
            signed_oid: field("SignedOid")?,
            signed_tid: field("SignedTid")?,
            signed_start: time("SignedStart")?,
            signed_expiry: time("SignedExpiry")?,
            signed_service: field("SignedService")?,
            signed_version: field("SignedVersion")?,
            signed_delegated_user_tenant_id: tag(xml, "SignedDelegatedUserTid").unwrap_or_default(),
            value: go_decoded_prefix(&field("Value")?),
        })
    }
}

fn go_decoded_prefix(text: &str) -> Vec<u8> {
    let compact: Vec<char> = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
    let mut decoded = Vec::new();
    for quantum in compact.chunks(4) {
        let quantum: String = quantum.iter().collect();
        match STANDARD.decode(&quantum) {
            Ok(bytes) => decoded.extend(bytes),
            Err(_) => break,
        }
        if quantum.contains('=') {
            break;
        }
    }
    decoded
}

fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?><UserDelegationKey><SignedOid>oid</SignedOid><SignedTid>tid</SignedTid><SignedStart>2026-01-02T03:04:05Z</SignedStart><SignedExpiry>2026-01-03T03:04:05.1234567Z</SignedExpiry><SignedService>b</SignedService><SignedVersion>2026-06-06</SignedVersion><Value>c2VjcmV0</Value></UserDelegationKey>"#;

    #[test]
    fn keys_parse_from_the_service_xml() {
        let key = UserDelegationKey::from_xml(KEY_XML).unwrap();

        assert_eq!(key.signed_oid, "oid");
        assert_eq!(key.signed_service, "b");
        assert_eq!(key.value, b"secret");
        assert_eq!(format_time(key.signed_start), "2026-01-02T03:04:05Z");
        assert_eq!(format_time(key.signed_expiry), "2026-01-03T03:04:05Z");
    }

    #[test]
    fn malformed_key_values_decode_up_to_the_first_bad_quantum_like_go() {
        assert_eq!(go_decoded_prefix("c2Vj\r\ncmV0"), b"secret");
        assert_eq!(go_decoded_prefix("c2Vj!!!!cmV0"), b"sec");
        assert_eq!(go_decoded_prefix("c2Vjcm"), b"sec");
        assert_eq!(go_decoded_prefix("c2Vjcg==cmV0"), b"secr");
        assert_eq!(go_decoded_prefix("!!"), b"");
    }

    #[tokio::test]
    async fn fetching_posts_the_key_info_with_the_bearer_token() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = crate::storage::backend::azure::testing::read_request(&mut connection).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{KEY_XML}",
                KEY_XML.len()
            );
            connection.write_all(response.as_bytes()).await.unwrap();
            request
        });
        let request = DelegationRequest {
            service_url: &format!("http://{address}/"),
            bearer: "aad-token",
            start: DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
            expiry: DateTime::parse_from_rfc3339("2026-01-03T03:04:05Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        let key = request.fetch(&Client::new()).await.unwrap();
        let sent = server.await.unwrap();

        assert_eq!(key.signed_tid, "tid");
        assert!(sent.starts_with("POST /?restype=service&comp=userdelegationkey HTTP/1.1"));
        assert!(sent.contains("authorization: Bearer aad-token"));
        assert!(sent.contains("x-ms-version: 2026-06-06"));
        assert!(
            sent.ends_with(
                "<KeyInfo><Start>2026-01-02T03:04:05Z</Start><Expiry>2026-01-03T03:04:05Z</Expiry></KeyInfo>"
            )
        );
    }
}
