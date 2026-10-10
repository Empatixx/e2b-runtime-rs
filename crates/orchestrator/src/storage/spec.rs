use std::fmt;

use crate::go_strconv::{parse_bool, quote};
use crate::go_url::GoUrl;
use crate::go_url::query::Query;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Gcp,
    Aws,
    Azure,
    Local,
}

impl Provider {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "GCPBucket" => Some(Provider::Gcp),
            "AWSBucket" => Some(Provider::Aws),
            "AzureBucket" => Some(Provider::Azure),
            "Local" => Some(Provider::Local),
            _ => None,
        }
    }

    fn bucket_scheme(&self) -> &'static str {
        match self {
            Provider::Aws => "s3",
            Provider::Azure => "azblob",
            Provider::Gcp | Provider::Local => "gs",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Provider::Gcp => "GCPBucket",
            Provider::Aws => "AWSBucket",
            Provider::Azure => "AzureBucket",
            Provider::Local => "Local",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageSpec {
    pub provider: Provider,
    pub bucket: String,
    pub base_path: String,
    pub endpoint: String,
    pub use_path_style: bool,
    pub region: String,
    pub server_side_encryption: String,
    pub sse_kms_key_id: String,
}

pub const SSE_AES256: &str = "AES256";
pub const SSE_AWS_KMS: &str = "aws:kms";

impl StorageSpec {
    pub fn local(base_path: &str) -> Self {
        Self {
            base_path: base_path.to_owned(),
            ..Self::empty(Provider::Local)
        }
    }

    fn empty(provider: Provider) -> Self {
        Self {
            provider,
            bucket: String::new(),
            base_path: String::new(),
            endpoint: String::new(),
            use_path_style: false,
            region: String::new(),
            server_side_encryption: String::new(),
            sse_kms_key_id: String::new(),
        }
    }

    pub fn parse_url(raw: &str) -> Result<Self, String> {
        let url = GoUrl::parse(raw.trim()).map_err(|e| format!("invalid storage URL: {e}"))?;
        match url.scheme.as_str() {
            "gs" => parse_bucket_url(&url, Provider::Gcp),
            "s3" => parse_bucket_url(&url, Provider::Aws),
            "azblob" => parse_bucket_url(&url, Provider::Azure),
            "file" => parse_file_url(&url),
            scheme => Err(format!(
                "storage URL {}: unsupported scheme {} (want gs, s3, azblob, or file)",
                redacted(&url),
                quote(scheme)
            )),
        }
    }

    pub fn bucket_url(provider: Provider, bucket: &str, s3_query: &Query) -> String {
        GoUrl {
            scheme: provider.bucket_scheme().to_owned(),
            host: bucket.to_owned(),
            raw_query: match provider {
                Provider::Aws => s3_query.encode(),
                _ => String::new(),
            },
            ..GoUrl::default()
        }
        .to_string()
    }

    pub fn file_url(base_path: &str) -> String {
        match base_path.starts_with('/') {
            true => GoUrl {
                scheme: "file".to_owned(),
                path: base_path.to_owned(),
                ..GoUrl::default()
            },
            false => GoUrl {
                scheme: "file".to_owned(),
                opaque: base_path.to_owned(),
                ..GoUrl::default()
            },
        }
        .to_string()
    }
}

fn redacted(url: &GoUrl) -> String {
    let query = url.query();
    let raw_query = match url.raw_query.is_empty() {
        true => String::new(),
        false => Query {
            values: query
                .values
                .into_iter()
                .map(|(key, values)| (key, values.iter().map(|value| without_user(value)).collect()))
                .collect(),
        }
        .encode(),
    };
    quote(
        &GoUrl {
            user: None,
            raw_query,
            ..url.clone()
        }
        .to_string(),
    )
}

fn without_user(value: &str) -> String {
    match GoUrl::parse(value) {
        Ok(url) if url.user.is_some() => GoUrl { user: None, ..url }.to_string(),
        _ => value.to_owned(),
    }
}

fn parse_bucket_url(url: &GoUrl, provider: Provider) -> Result<StorageSpec, String> {
    validate_bucket_url(url)?;
    let query = url.query();
    if provider != Provider::Aws && !query.is_empty() {
        return Err(format!(
            "storage URL {}: {}:// does not accept query parameters",
            redacted(url),
            url.scheme
        ));
    }
    query
        .values
        .iter()
        .try_fold(
            StorageSpec {
                bucket: url.host.clone(),
                ..StorageSpec::empty(provider)
            },
            |spec, (key, values)| apply_parameter(url, spec, key, values.last().map_or("", String::as_str)),
        )
        .and_then(|spec| checked_kms_key(url, spec))
}

fn checked_kms_key(url: &GoUrl, spec: StorageSpec) -> Result<StorageSpec, String> {
    match spec.sse_kms_key_id.is_empty() || spec.server_side_encryption == SSE_AWS_KMS {
        true => Ok(spec),
        false => Err(format!(
            "storage URL {}: kmskeyid requires ssetype={SSE_AWS_KMS}",
            redacted(url)
        )),
    }
}

fn apply_parameter(url: &GoUrl, spec: StorageSpec, key: &str, value: &str) -> Result<StorageSpec, String> {
    match key {
        "endpoint" => Ok(StorageSpec {
            endpoint: checked_endpoint(url, value)?,
            ..spec
        }),
        "s3ForcePathStyle" => Ok(StorageSpec {
            use_path_style: parse_bool(value).map_err(|e| {
                format!(
                    "storage URL {}: invalid s3ForcePathStyle {}: {e}",
                    redacted(url),
                    quote(value)
                )
            })?,
            ..spec
        }),
        "region" => Ok(StorageSpec {
            region: value.to_owned(),
            ..spec
        }),
        "ssetype" => match value {
            SSE_AES256 | SSE_AWS_KMS => Ok(StorageSpec {
                server_side_encryption: value.to_owned(),
                ..spec
            }),
            _ => Err(format!(
                "storage URL {}: invalid ssetype {} (want {SSE_AES256} or {SSE_AWS_KMS})",
                redacted(url),
                quote(value)
            )),
        },
        "kmskeyid" => Ok(StorageSpec {
            sse_kms_key_id: value.to_owned(),
            ..spec
        }),
        key => Err(format!(
            "storage URL {}: unsupported query parameter {} (want endpoint, s3ForcePathStyle, region, ssetype, or kmskeyid)",
            redacted(url),
            quote(key)
        )),
    }
}

fn checked_endpoint(url: &GoUrl, value: &str) -> Result<String, String> {
    let endpoint = GoUrl::parse(value)
        .ok()
        .filter(|endpoint| matches!(endpoint.scheme.as_str(), "http" | "https") && !endpoint.host.is_empty())
        .ok_or_else(|| {
            format!(
                "storage URL {}: endpoint must be an absolute http(s) URL",
                redacted(url)
            )
        })?;
    match endpoint.user {
        Some(_) => Err(credentials_rejected(url)),
        None => Ok(value.to_owned()),
    }
}

fn parse_file_url(url: &GoUrl) -> Result<StorageSpec, String> {
    if !url.query().is_empty() {
        return Err(format!(
            "storage URL {}: file: does not accept query parameters",
            redacted(url)
        ));
    }
    if !url.opaque.is_empty() {
        return Ok(StorageSpec::local(&url.opaque));
    }
    if !url.host.is_empty() && url.host != "localhost" {
        return Err(format!(
            "storage URL {}: file:// must not have a host (use file:///abs/path)",
            redacted(url)
        ));
    }
    if url.path.is_empty() {
        return Err(format!("storage URL {}: file:// requires a path", redacted(url)));
    }
    Ok(StorageSpec::local(&url.path))
}

fn validate_bucket_url(url: &GoUrl) -> Result<(), String> {
    if url.host.is_empty() {
        return Err(format!("storage URL {}: missing bucket name", redacted(url)));
    }
    if !url.path.is_empty() && url.path != "/" {
        return Err(format!(
            "storage URL {}: key prefixes are not supported (bucket only)",
            redacted(url)
        ));
    }
    match url.user {
        Some(_) => Err(credentials_rejected(url)),
        None => Ok(()),
    }
}

fn credentials_rejected(url: &GoUrl) -> String {
    format!("storage URL {}: credentials in URLs are not supported", redacted(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(provider: Provider, bucket: &str) -> StorageSpec {
        StorageSpec {
            bucket: bucket.to_owned(),
            ..StorageSpec::empty(provider)
        }
    }

    #[test]
    fn parses_every_scheme() {
        assert_eq!(
            StorageSpec::parse_url(" gs://templates "),
            Ok(bucket(Provider::Gcp, "templates"))
        );
        assert_eq!(StorageSpec::parse_url("azblob://c/"), Ok(bucket(Provider::Azure, "c")));
        assert_eq!(
            StorageSpec::parse_url("s3://b?endpoint=http://minio:9000/s3&s3ForcePathStyle=true&region=eu"),
            Ok(StorageSpec {
                endpoint: "http://minio:9000/s3".to_owned(),
                use_path_style: true,
                region: "eu".to_owned(),
                ..bucket(Provider::Aws, "b")
            })
        );
        assert_eq!(
            StorageSpec::parse_url("file:///var/lib/storage"),
            Ok(StorageSpec::local("/var/lib/storage"))
        );
        assert_eq!(
            StorageSpec::parse_url("file://localhost/x"),
            Ok(StorageSpec::local("/x"))
        );
        assert_eq!(
            StorageSpec::parse_url("file:rel/path"),
            Ok(StorageSpec::local("rel/path"))
        );
    }

    #[test]
    fn parses_server_side_encryption() {
        let sse = |raw: &str| StorageSpec::parse_url(raw);
        assert_eq!(
            sse("s3://my-bucket?ssetype=AES256"),
            Ok(StorageSpec {
                server_side_encryption: SSE_AES256.to_owned(),
                ..bucket(Provider::Aws, "my-bucket")
            })
        );
        assert_eq!(
            sse("s3://my-bucket?ssetype=aws:kms&kmskeyid=arn:aws:kms:us-east-1:111111111111:key/test"),
            Ok(StorageSpec {
                server_side_encryption: SSE_AWS_KMS.to_owned(),
                sse_kms_key_id: "arn:aws:kms:us-east-1:111111111111:key/test".to_owned(),
                ..bucket(Provider::Aws, "my-bucket")
            })
        );
        assert_eq!(
            sse("s3://my-bucket?ssetype=aws:kms"),
            Ok(StorageSpec {
                server_side_encryption: SSE_AWS_KMS.to_owned(),
                ..bucket(Provider::Aws, "my-bucket")
            })
        );
        assert!(
            sse("s3://my-bucket?ssetype=aes256")
                .unwrap_err()
                .contains("invalid ssetype \"aes256\"")
        );
        assert!(
            sse("s3://my-bucket?ssetype=AES256&kmskeyid=arn:aws:kms:us-east-1:111111111111:key/test")
                .unwrap_err()
                .contains("kmskeyid requires ssetype=aws:kms")
        );
        assert!(
            sse("gs://my-bucket?ssetype=AES256")
                .unwrap_err()
                .contains("does not accept query parameters")
        );
    }

    #[test]
    fn rejects_like_upstream() {
        let error = |raw: &str| StorageSpec::parse_url(raw).unwrap_err();
        assert_eq!(error("::"), "invalid storage URL: missing protocol scheme");
        assert_eq!(
            error("ftp://x"),
            "storage URL \"ftp://x\": unsupported scheme \"ftp\" (want gs, s3, azblob, or file)"
        );
        assert_eq!(error("gs://"), "storage URL \"gs:\": missing bucket name");
        assert_eq!(
            error("gs://b/prefix"),
            "storage URL \"gs://b/prefix\": key prefixes are not supported (bucket only)"
        );
        assert_eq!(
            error("s3://key:secret@b"),
            "storage URL \"s3://b\": credentials in URLs are not supported"
        );
        assert_eq!(
            error("gs://b?region=x"),
            "storage URL \"gs://b?region=x\": gs:// does not accept query parameters"
        );
        assert_eq!(
            error("s3://b?endpoint=minio:9000"),
            "storage URL \"s3://b?endpoint=minio%3A9000\": endpoint must be an absolute http(s) URL"
        );
        assert_eq!(
            error("s3://b?endpoint=http://k:s@minio"),
            "storage URL \"s3://b?endpoint=http%3A%2F%2Fminio\": credentials in URLs are not supported"
        );
        assert_eq!(
            error("s3://b?s3ForcePathStyle=yes"),
            "storage URL \"s3://b?s3ForcePathStyle=yes\": invalid s3ForcePathStyle \"yes\": strconv.ParseBool: parsing \"yes\": invalid syntax"
        );
        assert_eq!(
            error("s3://b?prefix=x"),
            "storage URL \"s3://b?prefix=x\": unsupported query parameter \"prefix\" (want endpoint, s3ForcePathStyle, region, ssetype, or kmskeyid)"
        );
        assert_eq!(
            error("file:x?y=1"),
            "storage URL \"file:x?y=1\": file: does not accept query parameters"
        );
        assert_eq!(
            error("file://host/x"),
            "storage URL \"file://host/x\": file:// must not have a host (use file:///abs/path)"
        );
        assert_eq!(error("file://"), "storage URL \"file:\": file:// requires a path");
    }

    #[test]
    fn builds_legacy_urls() {
        assert_eq!(
            StorageSpec::bucket_url(Provider::Aws, "b", &Query::single("s3ForcePathStyle", "true")),
            "s3://b?s3ForcePathStyle=true"
        );
        let path_style = Query::single("s3ForcePathStyle", "true");
        assert_eq!(StorageSpec::bucket_url(Provider::Gcp, "b", &path_style), "gs://b");
        assert_eq!(
            StorageSpec::bucket_url(Provider::Azure, "c", &Query::default()),
            "azblob://c"
        );
        assert_eq!(StorageSpec::file_url("/tmp/templates"), "file:///tmp/templates");
        assert_eq!(StorageSpec::file_url("rel"), "file:rel");
    }
}
