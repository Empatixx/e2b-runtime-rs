use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::azure::{MicrosoftAzure, MicrosoftAzureBuilder};
use object_store::gcp::{GoogleCloudStorage, GoogleCloudStorageBuilder};

use crate::go_url::GoUrl;
use crate::storage::spec::StorageSpec;

pub fn s3(spec: &StorageSpec, builder: AmazonS3Builder) -> Result<AmazonS3, String> {
    let builder = builder
        .with_bucket_name(&spec.bucket)
        .with_virtual_hosted_style_request(!spec.use_path_style);
    let builder = match spec.region.as_str() {
        "" => builder,
        region => builder.with_region(region),
    };
    let builder = match spec.endpoint.as_str() {
        "" => builder,
        endpoint => builder
            .with_endpoint(endpoint_for(spec, endpoint))
            .with_allow_http(endpoint.starts_with("http://")),
    };
    builder.build().map_err(|e| e.to_string())
}

fn endpoint_for(spec: &StorageSpec, endpoint: &str) -> String {
    match (spec.use_path_style, GoUrl::parse(endpoint)) {
        (false, Ok(url)) => GoUrl {
            host: format!("{}.{}", spec.bucket, url.host),
            ..url
        }
        .to_string(),
        _ => endpoint.to_owned(),
    }
}

pub fn gcs(spec: &StorageSpec, service_account_base64: Option<&str>) -> Result<GoogleCloudStorage, String> {
    let builder = GoogleCloudStorageBuilder::from_env().with_bucket_name(&spec.bucket);
    let builder = match service_account_base64.filter(|secret| !secret.is_empty()) {
        Some(secret) => builder.with_service_account_key(decode_service_account(secret)?),
        None => builder,
    };
    builder.build().map_err(|e| format!("failed to create GCS client: {e}"))
}

fn decode_service_account(secret: &str) -> Result<String, String> {
    STANDARD
        .decode(secret)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| "failed to parse GCP service account: invalid base64".to_owned())
}

pub struct AzureEnv<'a> {
    pub connection_string: Option<&'a str>,
    pub account_name: Option<&'a str>,
}

pub fn azure(spec: &StorageSpec, env: AzureEnv) -> Result<MicrosoftAzure, String> {
    let builder = MicrosoftAzureBuilder::from_env().with_container_name(&spec.bucket);
    let builder = match env.connection_string.filter(|value| !value.is_empty()) {
        Some(connection_string) => with_connection_string(builder, connection_string),
        None if env.account_name.is_none_or(str::is_empty) => {
            return Err("AZURE_STORAGE_ACCOUNT_NAME environment variable is not set".to_owned());
        }
        None => builder,
    };
    builder
        .build()
        .map_err(|e| format!("failed to create Azure client: {e}"))
}

fn with_connection_string(builder: MicrosoftAzureBuilder, connection_string: &str) -> MicrosoftAzureBuilder {
    connection_string
        .split(';')
        .filter_map(|part| part.split_once('='))
        .fold(builder, |builder, (key, value)| match key.trim() {
            "AccountName" => builder.with_account(value),
            "AccountKey" => builder.with_access_key(value),
            "BlobEndpoint" => builder.with_endpoint(value.to_owned()),
            _ => builder,
        })
}

#[cfg(test)]
mod tests {
    use crate::storage::spec::Provider;

    use super::*;

    fn s3_spec(path_style: bool) -> StorageSpec {
        StorageSpec {
            provider: Provider::Aws,
            bucket: "b".to_owned(),
            endpoint: "http://minio:9000".to_owned(),
            use_path_style: path_style,
            ..StorageSpec::local("")
        }
    }

    #[test]
    fn virtual_hosted_endpoints_carry_the_bucket() {
        assert_eq!(
            endpoint_for(&s3_spec(false), "http://minio:9000"),
            "http://b.minio:9000"
        );
        assert_eq!(endpoint_for(&s3_spec(true), "http://minio:9000"), "http://minio:9000");
    }

    #[test]
    fn azure_needs_an_account() {
        let spec = StorageSpec {
            provider: Provider::Azure,
            bucket: "c".to_owned(),
            ..StorageSpec::local("")
        };
        let error = azure(
            &spec,
            AzureEnv {
                connection_string: None,
                account_name: None,
            },
        )
        .unwrap_err();

        assert_eq!(error, "AZURE_STORAGE_ACCOUNT_NAME environment variable is not set");
    }
}
