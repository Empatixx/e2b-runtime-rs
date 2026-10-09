use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};

use super::delegation::UserDelegationKey;
use crate::go_url::query::Query;

pub const SERVICE_VERSION: &str = "2026-06-06";
const PERMISSIONS: &str = "cw";
const BLOB_RESOURCE: &str = "b";
const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";
const CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);
const HTTPS_ONLY: &str = "https";
const HTTPS_AND_HTTP: &str = "https,http";

#[derive(Debug, Clone)]
pub struct BlobSas {
    pub account: String,
    pub container: String,
    pub blob: String,
    pub https_only: bool,
    pub start: DateTime<Utc>,
    pub expiry: DateTime<Utc>,
}

pub enum SasKey<'a> {
    Account(&'a [u8]),
    Delegation(&'a UserDelegationKey),
}

impl BlobSas {
    pub fn issued_now(account: &str, container: &str, blob: &str, https_only: bool, ttl: Duration) -> Self {
        let now = DateTime::<Utc>::from(SystemTime::now());
        Self {
            account: account.to_owned(),
            container: container.to_owned(),
            blob: blob.to_owned(),
            https_only,
            start: now - CLOCK_SKEW,
            expiry: now + ttl,
        }
    }

    pub fn query(&self, key: SasKey) -> String {
        let signature = self.signature(&key);
        let mut query = Query::default();
        for (name, value) in self.parameters(&key) {
            query.values.insert(name.to_owned(), vec![value]);
        }
        query.values.insert("sig".to_owned(), vec![signature]);
        query.encode()
    }

    fn parameters(&self, key: &SasKey) -> Vec<(&'static str, String)> {
        let mut parameters = vec![
            ("sv", SERVICE_VERSION.to_owned()),
            ("spr", self.protocol().to_owned()),
            ("st", format_time(self.start)),
            ("se", format_time(self.expiry)),
            ("sr", BLOB_RESOURCE.to_owned()),
            ("sp", PERMISSIONS.to_owned()),
        ];
        if let SasKey::Delegation(delegation) = key {
            parameters.extend([
                ("skoid", delegation.signed_oid.clone()),
                ("sktid", delegation.signed_tid.clone()),
                ("skt", format_time(delegation.signed_start)),
                ("ske", format_time(delegation.signed_expiry)),
                ("sks", delegation.signed_service.clone()),
                ("skv", delegation.signed_version.clone()),
            ]);
        }
        parameters
    }

    fn protocol(&self) -> &'static str {
        match self.https_only {
            true => HTTPS_ONLY,
            false => HTTPS_AND_HTTP,
        }
    }

    fn canonical_name(&self) -> String {
        format!(
            "/blob/{}/{}/{}",
            self.account,
            self.container,
            self.blob.replace('\\', "/")
        )
    }

    fn signature(&self, key: &SasKey) -> String {
        match key {
            SasKey::Account(secret) => hmac_base64(secret, &self.service_string_to_sign()),
            SasKey::Delegation(delegation) => {
                hmac_base64(&delegation.value, &self.delegation_string_to_sign(delegation))
            }
        }
    }

    fn service_string_to_sign(&self) -> String {
        [
            PERMISSIONS,
            &format_time(self.start),
            &format_time(self.expiry),
            &self.canonical_name(),
            "",
            "",
            self.protocol(),
            SERVICE_VERSION,
            BLOB_RESOURCE,
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]
        .join("\n")
    }

    fn delegation_string_to_sign(&self, delegation: &UserDelegationKey) -> String {
        [
            PERMISSIONS,
            &format_time(self.start),
            &format_time(self.expiry),
            &self.canonical_name(),
            &delegation.signed_oid,
            &delegation.signed_tid,
            &format_time(delegation.signed_start),
            &format_time(delegation.signed_expiry),
            &delegation.signed_service,
            &delegation.signed_version,
            "",
            "",
            "",
            &delegation.signed_delegated_user_tenant_id,
            "",
            "",
            self.protocol(),
            SERVICE_VERSION,
            BLOB_RESOURCE,
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ]
        .join("\n")
    }
}

pub fn format_time(time: DateTime<Utc>) -> String {
    time.format(TIME_FORMAT).to_string()
}

fn hmac_base64(key: &[u8], message: &str) -> String {
    let signature = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), message.as_bytes());
    STANDARD.encode(signature)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn sas(https_only: bool) -> BlobSas {
        BlobSas {
            account: "acct".to_owned(),
            container: "box".to_owned(),
            blob: "templates/a.tar".to_owned(),
            https_only,
            start: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
            expiry: Utc.with_ymd_and_hms(2026, 1, 2, 4, 4, 5).unwrap(),
        }
    }

    #[test]
    fn the_service_string_to_sign_has_sixteen_fields_in_sdk_order() {
        let text = sas(true).service_string_to_sign();

        assert_eq!(
            text,
            "cw\n2026-01-02T03:04:05Z\n2026-01-02T04:04:05Z\n/blob/acct/box/templates/a.tar\n\n\nhttps\n2026-06-06\nb\n\n\n\n\n\n\n"
        );
        assert_eq!(text.split('\n').count(), 16);
    }

    #[test]
    fn account_key_queries_are_sorted_and_signed() {
        let query = sas(false).query(SasKey::Account(b"key"));

        assert!(query.starts_with("se=2026-01-02T04%3A04%3A05Z&sig="));
        assert!(query.ends_with("&sp=cw&spr=https%2Chttp&sr=b&st=2026-01-02T03%3A04%3A05Z&sv=2026-06-06"));
    }

    #[test]
    fn the_signature_is_an_hmac_of_the_string_to_sign() {
        let expected = hmac_base64(b"key", &sas(true).service_string_to_sign());

        let query = sas(true).query(SasKey::Account(b"key"));

        assert!(query.contains(&format!(
            "sig={}",
            expected.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D")
        )));
    }

    #[test]
    fn delegation_queries_carry_the_key_fields() {
        let delegation = UserDelegationKey {
            signed_oid: "oid".to_owned(),
            signed_tid: "tid".to_owned(),
            signed_start: Utc.with_ymd_and_hms(2026, 1, 2, 3, 0, 0).unwrap(),
            signed_expiry: Utc.with_ymd_and_hms(2026, 1, 3, 3, 0, 0).unwrap(),
            signed_service: "b".to_owned(),
            signed_version: "2026-06-06".to_owned(),
            signed_delegated_user_tenant_id: String::new(),
            value: b"secret".to_vec(),
        };

        let query = sas(true).query(SasKey::Delegation(&delegation));

        assert!(query.contains("skoid=oid&sks=b&skt=2026-01-02T03%3A00%3A00Z&sktid=tid&skv=2026-06-06"));
        assert!(query.contains("ske=2026-01-03T03%3A00%3A00Z"));
        assert_eq!(sas(true).delegation_string_to_sign(&delegation).split('\n').count(), 28);
    }
}
