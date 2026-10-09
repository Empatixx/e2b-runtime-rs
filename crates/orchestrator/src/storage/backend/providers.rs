use std::sync::Arc;

use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use object_store::ClientOptions;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::gcp::{GcpCredentialProvider, GoogleCloudStorage, GoogleCloudStorageBuilder};

use super::aws::{AwsDefaults, DefaultChainCredentials};
use super::blocking;
use super::gcs::detect::detect_default;
use super::gcs::flows::{CLOUD_PLATFORM_SCOPE, FlowCredentials, FlowProfile};
use super::gcs::oauth2::MultipartCredentials;
use super::gcs::store::GcsStore;
use super::gcs::universe::{client_universe_domain, storage_base_url};
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

const GCS_DEFAULT_AUTH_SCOPES: [&str; 5] = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/cloud-platform.read-only",
    "https://www.googleapis.com/auth/devstorage.full_control",
    "https://www.googleapis.com/auth/devstorage.read_only",
    "https://www.googleapis.com/auth/devstorage.read_write",
];
const QUOTA_PROJECT_HEADER: &str = "x-goog-user-project";

pub fn gcs(spec: &StorageSpec) -> Result<GcsStore, String> {
    let profile = FlowProfile::new(&GCS_DEFAULT_AUTH_SCOPES, true);
    let detected = blocking::run(detect_default(profile))
        .ok_or_else(|| gcs_error("credential detection did not finish"))?
        .map_err(|e| gcs_error(&e))?;
    let default_path = gcs_client(
        spec,
        Arc::new(FlowCredentials::new(detected.flow)),
        detected.quota_project.as_deref(),
    )?;
    let multipart_path = gcs_client(spec, Arc::new(MultipartCredentials::new(&[CLOUD_PLATFORM_SCOPE])), None)?;
    Ok(GcsStore::new(default_path, multipart_path))
}

fn gcs_client(
    spec: &StorageSpec,
    credentials: GcpCredentialProvider,
    quota_project: Option<&str>,
) -> Result<GoogleCloudStorage, String> {
    let builder = GoogleCloudStorageBuilder::new()
        .with_bucket_name(&spec.bucket)
        .with_credentials(credentials)
        .with_client_options(client_options(quota_project)?);
    let builder = match storage_base_url(&client_universe_domain()) {
        Some(base_url) => builder.with_base_url(&base_url),
        None => builder,
    };
    builder.build().map_err(|e| gcs_error(&e.to_string()))
}

fn client_options(quota_project: Option<&str>) -> Result<ClientOptions, String> {
    let headers = quota_project
        .map(|project| {
            HeaderValue::from_str(project)
                .map(|value| HeaderMap::from_iter([(HeaderName::from_static(QUOTA_PROJECT_HEADER), value)]))
                .map_err(|e| e.to_string())
        })
        .transpose()?
        .unwrap_or_default();
    Ok(ClientOptions::new().with_default_headers(headers))
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
