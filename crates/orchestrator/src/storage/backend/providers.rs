use std::sync::Arc;

use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::gcp::{GoogleCloudStorage, GoogleCloudStorageBuilder};

use super::aws::{AwsDefaults, DefaultChainCredentials};
use super::gcs::adc::AdcFile;
use super::gcs::external_account::{ExternalAccountCredentials, is_external_account};
use crate::go_url::GoUrl;
use crate::storage::spec::StorageSpec;

pub fn default_s3_builder(spec: &StorageSpec) -> AmazonS3Builder {
    let builder = AmazonS3Builder::from_env().with_credentials(Arc::new(DefaultChainCredentials::default()));
    match spec.region.is_empty() {
        true => AwsDefaults::blocking_region().map_or(builder.clone(), |region| builder.with_region(region)),
        false => builder,
    }
}

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

const GCS_SCOPES: [&str; 2] = [
    "https://www.googleapis.com/auth/devstorage.full_control",
    "https://www.googleapis.com/auth/cloud-platform",
];

pub fn gcs(spec: &StorageSpec) -> Result<GoogleCloudStorage, String> {
    let builder = GoogleCloudStorageBuilder::new().with_bucket_name(&spec.bucket);
    let adc = AdcFile::locate(&|name| std::env::var(name).ok()).map_err(|e| gcs_error(&e))?;
    let builder = match adc {
        Some(file) if is_external_account(&file.bytes) => {
            let credentials =
                ExternalAccountCredentials::from_json(&file.bytes, &GCS_SCOPES).map_err(|e| gcs_error(&e))?;
            builder.with_credentials(Arc::new(credentials))
        }
        Some(file) if file.from_environment => builder.with_application_credentials(file.path.display().to_string()),
        _ => builder,
    };
    builder.build().map_err(|e| gcs_error(&e.to_string()))
}

fn gcs_error(reason: &str) -> String {
    format!("failed to create GCS client: {reason}")
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
}
