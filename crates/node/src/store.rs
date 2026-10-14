use std::path::Path;

use anyhow::{Context, Result};
use opendal::{ErrorKind, Operator, services};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncReadExt;

use crate::config::S3Config;

const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;

pub struct Store {
    operator: Operator,
    bucket: String,
}

pub struct Versioned<T> {
    pub value: T,
    etag: String,
}

impl Store {
    pub fn new(config: &S3Config) -> Result<Self> {
        let builder = services::S3::default()
            .endpoint(&config.endpoint)
            .bucket(&config.bucket)
            .region(&config.region)
            .access_key_id(&config.access_key)
            .secret_access_key(&config.secret_key);
        Ok(Self {
            operator: Operator::new(builder)?.finish(),
            bucket: config.bucket.clone(),
        })
    }

    pub fn url(&self, prefix: &str) -> String {
        format!("s3://{}/{prefix}", self.bucket)
    }

    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<Versioned<T>>> {
        let stat = match self.operator.stat(key).await {
            Ok(stat) => stat,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(format!("stat {key}")),
        };
        let etag = stat.etag().context("object without etag")?.to_owned();
        let body = self.operator.read_with(key).if_match(&etag).await?;
        Ok(Some(Versioned {
            value: serde_json::from_slice(&body.to_vec())?,
            etag,
        }))
    }

    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.operator.write(key, serde_json::to_vec_pretty(value)?).await?;
        Ok(())
    }

    pub async fn create<T: Serialize>(&self, key: &str, value: &T) -> Result<bool> {
        let written = self
            .operator
            .write_with(key, serde_json::to_vec_pretty(value)?)
            .if_not_exists(true)
            .await;
        succeeded_unless_conflict(written)
    }

    pub async fn replace<T: Serialize, U>(&self, key: &str, current: &Versioned<U>, value: &T) -> Result<bool> {
        let written = self
            .operator
            .write_with(key, serde_json::to_vec_pretty(value)?)
            .if_match(&current.etag)
            .await;
        succeeded_unless_conflict(written)
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        Ok(self.operator.delete(key).await?)
    }

    pub async fn list<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<T>> {
        let mut values = Vec::new();
        for entry in self.operator.list(prefix).await? {
            let Some(found) = self.get(entry.path()).await? else {
                continue;
            };
            values.push(found.value);
        }
        Ok(values)
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.operator.exists(key).await?)
    }

    pub async fn upload(&self, key: &str, file: &Path) -> Result<()> {
        let mut source = tokio::fs::File::open(file).await?;
        let mut writer = self.operator.writer_with(key).chunk(UPLOAD_CHUNK).await?;
        let mut buffer = vec![0u8; UPLOAD_CHUNK];
        loop {
            let read = source.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            writer.write(buffer[..read].to_vec()).await?;
        }
        writer.close().await?;
        Ok(())
    }

    pub async fn download(&self, key: &str, file: &Path) -> Result<()> {
        let body = self
            .operator
            .read(key)
            .await
            .with_context(|| format!("download {key}"))?;
        std::fs::create_dir_all(file.parent().context("download target without parent")?)?;
        let tmp = file.with_extension("download");
        tokio::fs::write(&tmp, body.to_vec()).await?;
        tokio::fs::rename(tmp, file).await?;
        Ok(())
    }
}

fn succeeded_unless_conflict<T>(written: opendal::Result<T>) -> Result<bool> {
    match written {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::ConditionNotMatch | ErrorKind::AlreadyExists) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
