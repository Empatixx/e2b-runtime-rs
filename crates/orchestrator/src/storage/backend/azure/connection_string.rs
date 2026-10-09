use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::builder::oci::ecr::base64_error;

const MALFORMED: &str = "connection string is either blank or malformed. The expected connection string should contain key value pairs separated by semicolons. For example 'DefaultEndpointsProtocol=https;AccountName=<accountName>;AccountKey=<accountKey>;EndpointSuffix=core.windows.net'";
const NEEDS_ENDPOINT: &str = "connection string needs either AccountName or BlobEndpoint";
const NEEDS_CREDENTIAL: &str = "connection string needs either AccountKey or SharedAccessSignature";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedKey {
    pub account: String,
    pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionString {
    pub service_url: String,
    pub shared_key: Option<SharedKey>,
    pub shared_access_signature: Option<String>,
}

impl ConnectionString {
    pub fn parse(text: &str) -> Result<Self, String> {
        let entries = entries(text)?;
        let find = |name: &str| {
            entries
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let service_url = service_url(
            find("BlobEndpoint"),
            find("AccountName"),
            find("DefaultEndpointsProtocol").unwrap_or_else(|| "https".to_owned()),
            find("EndpointSuffix").unwrap_or_else(|| "core.windows.net".to_owned()),
        )?;
        match (find("AccountName"), find("AccountKey"), find("SharedAccessSignature")) {
            (Some(account), Some(key), _) => Ok(Self {
                service_url,
                shared_key: Some(SharedKey {
                    account,
                    key: decode_key(&key)?,
                }),
                shared_access_signature: None,
            }),
            (_, _, Some(signature)) => Ok(Self {
                service_url,
                shared_key: None,
                shared_access_signature: Some(signature),
            }),
            _ => Err(NEEDS_CREDENTIAL.to_owned()),
        }
    }

    pub fn account_from_host(&self) -> String {
        self.service_url
            .split_once("://")
            .map_or(self.service_url.as_str(), |(_, rest)| rest)
            .split(['/', '.', ':'])
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

fn entries(text: &str) -> Result<Vec<(String, String)>, String> {
    text.trim_end_matches(';')
        .split(';')
        .map(|part| {
            part.split_once('=')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .ok_or_else(|| MALFORMED.to_owned())
        })
        .collect()
}

fn service_url(
    blob_endpoint: Option<String>,
    account: Option<String>,
    protocol: String,
    suffix: String,
) -> Result<String, String> {
    let url = match (blob_endpoint, account) {
        (Some(endpoint), _) => endpoint,
        (None, Some(account)) => format!("{protocol}://{account}.blob.{suffix}"),
        (None, None) => return Err(NEEDS_ENDPOINT.to_owned()),
    };
    Ok(match url.ends_with('/') {
        true => url,
        false => format!("{url}/"),
    })
}

fn decode_key(key: &str) -> Result<Vec<u8>, String> {
    STANDARD
        .decode(key)
        .map_err(|e| format!("decode account key: {}", base64_error(key, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_keys_build_the_default_service_url() {
        let parsed = ConnectionString::parse("AccountName=acct;AccountKey=a2V5;").unwrap();

        assert_eq!(parsed.service_url, "https://acct.blob.core.windows.net/");
        assert_eq!(
            parsed.shared_key,
            Some(SharedKey {
                account: "acct".to_owned(),
                key: b"key".to_vec()
            })
        );
        assert_eq!(parsed.account_from_host(), "acct");
    }

    #[test]
    fn protocol_suffix_and_blob_endpoint_shape_the_url() {
        let suffixed = ConnectionString::parse(
            "DefaultEndpointsProtocol=http;EndpointSuffix=core.chinacloudapi.cn;AccountName=a;AccountKey=a2V5",
        )
        .unwrap();
        let emulator = ConnectionString::parse(
            "AccountName=devstoreaccount1;AccountKey=a2V5;BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1",
        )
        .unwrap();

        assert_eq!(suffixed.service_url, "http://a.blob.core.chinacloudapi.cn/");
        assert_eq!(emulator.service_url, "http://127.0.0.1:10000/devstoreaccount1/");
    }

    #[test]
    fn a_shared_access_signature_rides_without_a_key() {
        let parsed =
            ConnectionString::parse("BlobEndpoint=https://acct.blob.core.windows.net;SharedAccessSignature=sv=1&sig=x")
                .unwrap();

        assert_eq!(parsed.shared_key, None);
        assert_eq!(parsed.shared_access_signature.as_deref(), Some("sv=1&sig=x"));
        assert_eq!(parsed.account_from_host(), "acct");
    }

    #[test]
    fn broken_strings_fail_with_the_sdk_texts() {
        assert!(
            ConnectionString::parse("nonsense")
                .unwrap_err()
                .starts_with("connection string is either blank or malformed.")
        );
        assert_eq!(
            ConnectionString::parse("AccountKey=a2V5").unwrap_err(),
            "connection string needs either AccountName or BlobEndpoint"
        );
        assert_eq!(
            ConnectionString::parse("AccountName=a").unwrap_err(),
            "connection string needs either AccountKey or SharedAccessSignature"
        );
        assert_eq!(
            ConnectionString::parse("AccountName=a;AccountKey=a*b=").unwrap_err(),
            "decode account key: illegal base64 data at input byte 1"
        );
    }
}
