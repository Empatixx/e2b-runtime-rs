use std::collections::HashMap;

use hyper::header::{HeaderName, HeaderValue};
use object_store::signer::SignedUrlOptions;

use crate::storage::spec::StorageSpec;

const ALGORITHM_HEADER: &str = "X-Amz-Server-Side-Encryption";
const KMS_KEY_ID_HEADER: &str = "X-Amz-Server-Side-Encryption-Aws-Kms-Key-Id";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerSideEncryption {
    algorithm: String,
    kms_key_id: String,
}

impl ServerSideEncryption {
    pub fn of(spec: &StorageSpec) -> Self {
        Self {
            algorithm: spec.server_side_encryption.clone(),
            kms_key_id: spec.sse_kms_key_id.clone(),
        }
    }

    pub fn headers(&self) -> HashMap<String, String> {
        [
            (ALGORITHM_HEADER, &self.algorithm),
            (KMS_KEY_ID_HEADER, &self.kms_key_id),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| (name.to_owned(), value.clone()))
        .collect()
    }

    pub fn signed_url_options(&self) -> Result<SignedUrlOptions, String> {
        self.headers()
            .into_iter()
            .try_fold(SignedUrlOptions::default(), |options, (name, value)| {
                let name = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).map_err(|e| e.to_string())?;
                let value = HeaderValue::from_str(&value).map_err(|e| e.to_string())?;
                Ok(options.with_signed_header(name, value))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "arn:aws:kms:us-east-1:111111111111:key/test";

    fn encryption(algorithm: &str, kms_key_id: &str) -> ServerSideEncryption {
        ServerSideEncryption::of(&StorageSpec {
            server_side_encryption: algorithm.to_owned(),
            sse_kms_key_id: kms_key_id.to_owned(),
            ..StorageSpec::local("")
        })
    }

    #[test]
    fn no_encryption_sends_no_headers() {
        assert!(encryption("", "").headers().is_empty());
        assert!(encryption("", "").signed_url_options().unwrap().is_empty());
    }

    #[test]
    fn sse_s3_sends_only_the_algorithm() {
        assert_eq!(
            encryption("AES256", "").headers(),
            HashMap::from([(ALGORITHM_HEADER.to_owned(), "AES256".to_owned())])
        );
    }

    #[test]
    fn sse_kms_sends_the_algorithm_and_the_key() {
        assert_eq!(
            encryption("aws:kms", KEY).headers(),
            HashMap::from([
                (ALGORITHM_HEADER.to_owned(), "aws:kms".to_owned()),
                (KMS_KEY_ID_HEADER.to_owned(), KEY.to_owned()),
            ])
        );
    }

    fn sse_cases() -> Vec<(&'static str, ServerSideEncryption, Vec<(&'static str, &'static str)>)> {
        vec![
            ("none", encryption("", ""), vec![]),
            (
                "sse-s3",
                encryption("AES256", ""),
                vec![("x-amz-server-side-encryption", "AES256")],
            ),
            (
                "sse-kms with key",
                encryption("aws:kms", KEY),
                vec![
                    ("x-amz-server-side-encryption", "aws:kms"),
                    ("x-amz-server-side-encryption-aws-kms-key-id", KEY),
                ],
            ),
        ]
    }

    fn spec_of(encryption: &ServerSideEncryption) -> StorageSpec {
        StorageSpec {
            server_side_encryption: encryption.algorithm.clone(),
            sse_kms_key_id: encryption.kms_key_id.clone(),
            ..StorageSpec::local("")
        }
    }

    fn assert_recorded(recorded: &crate::storage::testing::RecordedRequests, operation: &str, want: &[(&str, &str)]) {
        let seen = recorded.lock().unwrap();
        let headers = seen
            .get(operation)
            .unwrap_or_else(|| panic!("no {operation} request recorded"));
        [
            "x-amz-server-side-encryption",
            "x-amz-server-side-encryption-aws-kms-key-id",
        ]
        .into_iter()
        .for_each(|name| {
            let expected = want
                .iter()
                .find(|(want_name, _)| *want_name == name)
                .map(|(_, value)| *value);
            assert_eq!(headers.get(name).map(String::as_str), expected, "{operation}: {name}");
        });
    }

    #[tokio::test]
    async fn put_requests_server_side_encryption() {
        for (_, encryption, want) in sse_cases() {
            let (storage, recorded) = crate::storage::testing::recording_s3(spec_of(&encryption)).await;

            storage
                .put("test-object", bytes::Bytes::from_static(b"payload"))
                .await
                .unwrap();

            assert_recorded(&recorded, "PutObject", &want);
        }
    }

    #[tokio::test]
    async fn put_file_requests_server_side_encryption() {
        for (_, encryption, want) in sse_cases() {
            let (storage, recorded) = crate::storage::testing::recording_s3(spec_of(&encryption)).await;
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("small");
            std::fs::write(&source, b"small-uncompressed-file").unwrap();

            storage.put_file("test-object", &source).await.unwrap();

            assert_recorded(&recorded, "PutObject", &want);
        }
    }

    #[tokio::test]
    async fn a_multipart_upload_requests_server_side_encryption_at_initiation() {
        for (_, encryption, want) in sse_cases() {
            let (storage, recorded) = crate::storage::testing::recording_s3(spec_of(&encryption)).await;
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("large");
            std::fs::write(&source, vec![7u8; 12 << 20]).unwrap();

            storage.put_file("test-object", &source).await.unwrap();

            assert_recorded(&recorded, "CreateMultipartUpload", &want);
        }
    }

    #[tokio::test]
    async fn the_upload_url_returns_the_signed_server_side_encryption_headers() {
        for (_, encryption, want) in sse_cases() {
            let (storage, _) = crate::storage::testing::recording_s3(spec_of(&encryption)).await;

            let upload = storage
                .upload_signed_url("test-object", std::time::Duration::from_secs(3600))
                .await
                .unwrap();

            let expected: HashMap<String, String> = want
                .iter()
                .map(|(name, value)| (canonical(name), (*value).to_owned()))
                .collect();
            assert_eq!(upload.headers, expected);
            let url = url::Url::parse(&upload.url).unwrap();
            let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
            let signed: Vec<&str> = query["X-Amz-SignedHeaders"].split(';').collect();
            want.iter().for_each(|(name, _)| {
                assert!(
                    signed.contains(name),
                    "{name} must be signed, not hoisted into the query"
                );
                assert!(!query.contains_key(*name), "{name} must not be a query parameter");
            });
        }
    }

    fn canonical(name: &str) -> String {
        name.split('-')
            .map(|part| part[..1].to_ascii_uppercase() + &part[1..])
            .collect::<Vec<_>>()
            .join("-")
    }
}
