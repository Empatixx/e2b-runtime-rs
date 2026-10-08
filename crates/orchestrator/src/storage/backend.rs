use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use hyper::Method;
use object_store::aws::AmazonS3Builder;
use object_store::buffered::BufWriter;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::signer::Signer;
use object_store::{GetOptions, ObjectStore, ObjectStoreExt, PutPayload};

use tokio::io::AsyncWriteExt;

use super::header::Header;

#[derive(Clone)]
pub struct Storage {
    store: Arc<dyn ObjectStore>,
    signer: Option<Arc<dyn Signer>>,
}

#[derive(Default)]
pub struct S3Builder {
    bucket: String,
    endpoint: Option<String>,
    key: String,
    secret: String,
    region: String,
}

impl Storage {
    pub fn local(root: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(root).map_err(|e| format!("create storage dir {}: {e}", root.display()))?;
        let store = LocalFileSystem::new_with_prefix(root).map_err(|e| e.to_string())?;
        Ok(Self {
            store: Arc::new(store),
            signer: None,
        })
    }

    pub fn s3() -> S3Builder {
        S3Builder::default()
    }

    pub async fn put(&self, path: &str, data: Bytes) -> Result<(), String> {
        self.store
            .put(&ObjectPath::from(path), PutPayload::from(data))
            .await
            .map(|_| ())
            .map_err(|e| format!("put {path}: {e}"))
    }

    pub async fn put_file(&self, path: &str, file: &Path) -> Result<(), String> {
        let mut source = tokio::fs::File::open(file)
            .await
            .map_err(|e| format!("open {}: {e}", file.display()))?;
        let mut writer = BufWriter::new(self.store.clone(), ObjectPath::from(path));
        tokio::io::copy(&mut source, &mut writer)
            .await
            .map_err(|e| format!("put {path}: {e}"))?;
        writer.shutdown().await.map_err(|e| format!("put {path}: {e}"))
    }

    pub async fn get(&self, path: &str) -> Result<Bytes, String> {
        let object = self
            .store
            .get(&ObjectPath::from(path))
            .await
            .map_err(|e| format!("get {path}: {e}"))?;
        object.bytes().await.map_err(|e| format!("read {path}: {e}"))
    }

    pub async fn find(&self, path: &str) -> Result<Option<Bytes>, String> {
        match self.store.get(&ObjectPath::from(path)).await {
            Ok(object) => object.bytes().await.map(Some).map_err(|e| format!("read {path}: {e}")),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(format!("get {path}: {e}")),
        }
    }

    pub async fn get_stream(
        &self,
        path: &str,
        range: Range<u64>,
    ) -> Result<BoxStream<'static, Result<Bytes, String>>, String> {
        let options = GetOptions {
            range: Some(range.clone().into()),
            ..GetOptions::default()
        };
        let object = self
            .store
            .get_opts(&ObjectPath::from(path), options)
            .await
            .map_err(|e| format!("get {path} {range:?}: {e}"))?;
        let path = path.to_owned();
        Ok(object
            .into_stream()
            .map_err(move |e| format!("read {path}: {e}"))
            .boxed())
    }

    pub async fn get_range(&self, path: &str, range: Range<u64>) -> Result<Bytes, String> {
        let bytes = self
            .store
            .get_range(&ObjectPath::from(path), range.clone())
            .await
            .map_err(|e| format!("get {path} {range:?}: {e}"))?;
        let expected = range.end - range.start;
        if bytes.len() as u64 != expected {
            return Err(format!("get {path} {range:?}: got {} of {expected} bytes", bytes.len()));
        }
        Ok(bytes)
    }

    pub async fn size(&self, path: &str) -> Result<u64, String> {
        self.store
            .head(&ObjectPath::from(path))
            .await
            .map(|meta| meta.size)
            .map_err(|e| format!("head {path}: {e}"))
    }

    pub async fn exists(&self, path: &str) -> Result<bool, String> {
        match self.store.head(&ObjectPath::from(path)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(format!("head {path}: {e}")),
        }
    }

    pub async fn delete_prefix(&self, prefix: &str) -> Result<(), String> {
        let prefix_path = ObjectPath::from(prefix);
        let objects: Vec<_> = self
            .store
            .list(Some(&prefix_path))
            .try_collect()
            .await
            .map_err(|e| format!("list {prefix}: {e}"))?;
        for object in objects {
            self.store
                .delete(&object.location)
                .await
                .map_err(|e| format!("delete {}: {e}", object.location))?;
        }
        Ok(())
    }

    pub async fn upload_signed_url(&self, path: &str, expires_in: Duration) -> Result<String, String> {
        let signer = self
            .signer
            .as_ref()
            .ok_or("failed to presign PUT URL: local storage cannot sign URLs")?;
        signer
            .signed_url(Method::PUT, &ObjectPath::from(path), expires_in)
            .await
            .map(|url| url.to_string())
            .map_err(|e| format!("failed to presign PUT URL: {e}"))
    }

    pub async fn load_header(&self, path: &str) -> Result<Header, String> {
        Header::decode(&self.get(path).await?)
    }

    pub async fn store_header(&self, path: &str, header: &Header) -> Result<(), String> {
        self.put(path, Bytes::from(header.encode())).await
    }
}

impl S3Builder {
    pub fn bucket(mut self, bucket: &str) -> Self {
        self.bucket = bucket.to_owned();
        self
    }

    pub fn endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = Some(endpoint.to_owned());
        self
    }

    pub fn credentials(mut self, key: &str, secret: &str) -> Self {
        self.key = key.to_owned();
        self.secret = secret.to_owned();
        self
    }

    pub fn region(mut self, region: &str) -> Self {
        self.region = region.to_owned();
        self
    }

    pub fn build(self) -> Result<Storage, String> {
        let builder = AmazonS3Builder::new()
            .with_bucket_name(&self.bucket)
            .with_access_key_id(&self.key)
            .with_secret_access_key(&self.secret)
            .with_region(&self.region);
        let builder = match &self.endpoint {
            Some(endpoint) => builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"))
                .with_virtual_hosted_style_request(false),
            None => builder,
        };
        let store = Arc::new(builder.build().map_err(|e| format!("s3 bucket {}: {e}", self.bucket))?);
        Ok(Storage {
            store: store.clone(),
            signer: Some(store),
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use uuid::Uuid;

    use super::*;
    use crate::storage::header::Metadata;

    async fn exercise(storage: &Storage, prefix: &str) {
        let path = format!("{prefix}/memfile");
        storage.put(&path, Bytes::from_static(b"0123456789")).await.unwrap();

        assert_eq!(storage.get(&path).await.unwrap(), Bytes::from_static(b"0123456789"));
        assert_eq!(
            storage.get_range(&path, 2..5).await.unwrap(),
            Bytes::from_static(b"234")
        );
        assert_eq!(storage.size(&path).await.unwrap(), 10);
        assert!(storage.get_range(&path, 8..20).await.is_err());

        let header = Header::new(Metadata::template(Uuid::from_u128(7), 4096, 8192), Vec::new()).unwrap();
        let header_path = format!("{prefix}/memfile.header");
        storage.store_header(&header_path, &header).await.unwrap();
        assert_eq!(storage.load_header(&header_path).await.unwrap(), header);

        storage.delete_prefix(prefix).await.unwrap();
        assert!(storage.get(&path).await.is_err());
        assert!(storage.get(&header_path).await.is_err());
    }

    #[tokio::test]
    async fn exists_tells_present_from_missing_objects() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        storage.put("b/present", Bytes::from_static(b"x")).await.unwrap();

        assert_eq!(storage.exists("b/present").await, Ok(true));
        assert_eq!(storage.exists("b/missing").await, Ok(false));
    }

    #[tokio::test]
    async fn put_file_streams_a_file_into_storage() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let content: Vec<u8> = (0..9 << 20).map(|i: u32| (i % 251) as u8).collect();
        let source = dir.path().join("rootfs.ext4");
        std::fs::write(&source, &content).unwrap();

        storage.put_file("t/rootfs.ext4", &source).await.unwrap();

        assert_eq!(storage.get("t/rootfs.ext4").await.unwrap(), Bytes::from(content));
    }

    #[tokio::test]
    async fn local_filesystem_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let storage = Storage::local(root.path()).unwrap();
        exercise(&storage, "build-a").await;
    }

    #[tokio::test]
    async fn s3_round_trip_when_minio_is_available() {
        let Ok(endpoint) = std::env::var("E2B_TEST_S3_ENDPOINT") else {
            return;
        };
        let storage = Storage::s3()
            .bucket("e2b-test")
            .endpoint(&endpoint)
            .credentials("minio", "minio123")
            .region("us-east-1")
            .build()
            .unwrap();
        exercise(&storage, &Uuid::new_v4().to_string()).await;
    }
}
