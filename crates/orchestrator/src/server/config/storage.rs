use std::collections::HashMap;

use crate::go_strconv::env_bool;
use crate::storage::spec::{Provider, StorageSpec};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageEnv {
    pub template_url: String,
    pub build_cache_url: String,
    pub provider: String,
    pub template_bucket: String,
    pub template_base_path: String,
    pub build_cache_bucket: String,
    pub build_cache_base_path: String,
    pub s3_path_style: bool,
}

struct Role<'a> {
    url: &'a str,
    bucket: &'a str,
    base_path: &'a str,
    name: &'a str,
    bucket_env: &'a str,
    default_base_path: &'a str,
}

impl StorageEnv {
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        let var = |name: &str| vars.get(name).cloned().unwrap_or_default();
        Ok(Self {
            template_url: var("TEMPLATE_STORAGE_URL"),
            build_cache_url: var("BUILD_CACHE_STORAGE_URL"),
            provider: var("STORAGE_PROVIDER"),
            template_bucket: var("TEMPLATE_BUCKET_NAME"),
            template_base_path: var("LOCAL_TEMPLATE_STORAGE_BASE_PATH"),
            build_cache_bucket: var("BUILD_CACHE_BUCKET_NAME"),
            build_cache_base_path: var("LOCAL_BUILD_CACHE_STORAGE_BASE_PATH"),
            s3_path_style: env_bool(vars.get("S3_USE_PATH_STYLE").map(String::as_str), "S3PathStyle")
                .map_err(|e| format!("parse storage environment: {e}"))?,
        })
    }

    pub fn template(&self) -> Result<StorageSpec, String> {
        self.resolve(Role {
            url: &self.template_url,
            bucket: &self.template_bucket,
            base_path: &self.template_base_path,
            name: "template",
            bucket_env: "TEMPLATE_BUCKET_NAME",
            default_base_path: "/tmp/templates",
        })
    }

    pub fn build_cache(&self) -> Result<StorageSpec, String> {
        self.resolve(Role {
            url: &self.build_cache_url,
            bucket: &self.build_cache_bucket,
            base_path: &self.build_cache_base_path,
            name: "build cache",
            bucket_env: "BUILD_CACHE_BUCKET_NAME",
            default_base_path: "/tmp/build-cache",
        })
    }

    fn resolve(&self, role: Role) -> Result<StorageSpec, String> {
        match role.url.trim() {
            "" => StorageSpec::parse_url(&self.legacy_url(&role)?),
            url => StorageSpec::parse_url(url),
        }
    }

    fn legacy_url(&self, role: &Role) -> Result<String, String> {
        let provider = match self.provider.as_str() {
            "" => Some(Provider::Gcp),
            name => Provider::parse(name),
        }
        .ok_or_else(|| format!("unknown storage provider: {}", self.provider))?;
        match (provider, role.bucket) {
            (Provider::Local, _) => Ok(StorageSpec::file_url(match role.base_path {
                "" => role.default_base_path,
                base_path => base_path,
            })),
            (_, "") => Err(format!(
                "{} storage bucket not configured: set {}",
                role.name, role.bucket_env
            )),
            (provider, bucket) => Ok(StorageSpec::bucket_url(provider, bucket, self.s3_path_style)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Result<StorageEnv, String> {
        StorageEnv::from_vars(
            &pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        )
    }

    #[test]
    fn urls_win_over_legacy_variables() {
        let env = env(&[
            ("TEMPLATE_STORAGE_URL", " s3://t?region=eu "),
            ("BUILD_CACHE_STORAGE_URL", "file:///cache"),
            ("STORAGE_PROVIDER", "nonsense"),
        ])
        .unwrap();

        assert_eq!(env.template().unwrap().bucket, "t");
        assert_eq!(env.build_cache(), Ok(StorageSpec::local("/cache")));
    }

    #[test]
    fn legacy_variables_convert_to_urls() {
        let local = env(&[
            ("STORAGE_PROVIDER", "Local"),
            ("LOCAL_TEMPLATE_STORAGE_BASE_PATH", "rel"),
        ])
        .unwrap();
        assert_eq!(local.template(), Ok(StorageSpec::local("rel")));
        assert_eq!(local.build_cache(), Ok(StorageSpec::local("/tmp/build-cache")));

        let aws = env(&[
            ("STORAGE_PROVIDER", "AWSBucket"),
            ("TEMPLATE_BUCKET_NAME", "t"),
            ("S3_USE_PATH_STYLE", "true"),
        ])
        .unwrap()
        .template()
        .unwrap();
        assert_eq!(
            (aws.provider, aws.bucket.as_str(), aws.use_path_style),
            (Provider::Aws, "t", true)
        );
    }

    #[test]
    fn legacy_errors_match_upstream() {
        assert_eq!(
            env(&[]).unwrap().template(),
            Err("template storage bucket not configured: set TEMPLATE_BUCKET_NAME".to_owned())
        );
        assert_eq!(
            env(&[("STORAGE_PROVIDER", "AzureBucket")]).unwrap().build_cache(),
            Err("build cache storage bucket not configured: set BUILD_CACHE_BUCKET_NAME".to_owned())
        );
        assert_eq!(
            env(&[("STORAGE_PROVIDER", "Disk")]).unwrap().template(),
            Err("unknown storage provider: Disk".to_owned())
        );
        assert_eq!(
            env(&[("S3_USE_PATH_STYLE", "yes")]).unwrap_err(),
            "parse storage environment: env: parse error on field \"S3PathStyle\" of type \"bool\": strconv.ParseBool: parsing \"yes\": invalid syntax"
        );
    }
}
