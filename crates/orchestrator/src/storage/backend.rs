mod providers;

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use hyper::Method;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::azure::MicrosoftAzure;
use object_store::buffered::BufWriter;
use object_store::gcp::GoogleCloudStorage;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::signer::Signer;
use object_store::{
    Attribute, AttributeValue, Attributes, GetOptions, ObjectStore, ObjectStoreExt, PutOptions, PutPayload,
};

use tokio::io::AsyncWriteExt;

use super::header::Header;
use super::local_upload::UploadSigner;
use super::spec::{Provider, StorageSpec};
use providers::AzureEnv;

const AZURE_BLOB_TYPE_HEADER: &str = "x-ms-blob-type";
const AZURE_BLOCK_BLOB: &str = "BlockBlob";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub entries: BTreeMap<String, String>,
}

impl ObjectMetadata {
    pub fn with(mut self, key: &str, value: impl ToString) -> Self {
        self.entries.insert(key.to_owned(), value.to_string());
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadUrl {
    pub url: String,
    pub headers: HashMap<String, String>,
}

#[derive(Clone)]
pub struct Storage {
    store: Arc<dyn ObjectStore>,
    signing: Signing,
}

#[derive(Clone)]
enum Signing {
    Local {
        root: PathBuf,
        upload: Option<UploadSigner>,
    },
    S3(Arc<AmazonS3>),
    Gcs(Arc<GoogleCloudStorage>),
    Azure(Arc<MicrosoftAzure>),
}

impl Storage {
    pub fn local(root: &Path) -> Result<Self, String> {
        Self::local_with(root, None)
    }

    pub fn open(spec: &StorageSpec, upload: Option<UploadSigner>) -> Result<Self, String> {
        match spec.provider {
            Provider::Local => Self::local_with(Path::new(&spec.base_path), upload),
            Provider::Aws => Self::s3(spec, AmazonS3Builder::from_env()),
            Provider::Gcp => providers::gcs(spec, std::env::var("GOOGLE_SERVICE_ACCOUNT_BASE64").ok().as_deref())
                .map(|store| Self::remote(Arc::new(store), Signing::Gcs)),
            Provider::Azure => providers::azure(
                spec,
                AzureEnv {
                    connection_string: std::env::var("AZURE_STORAGE_CONNECTION_STRING").ok().as_deref(),
                    account_name: std::env::var("AZURE_STORAGE_ACCOUNT_NAME").ok().as_deref(),
                },
            )
            .map(|store| Self::remote(Arc::new(store), Signing::Azure)),
        }
    }

    pub fn s3(spec: &StorageSpec, builder: AmazonS3Builder) -> Result<Self, String> {
        providers::s3(spec, builder).map(|store| Self::remote(Arc::new(store), Signing::S3))
    }

    fn remote<T: ObjectStore>(store: Arc<T>, signing: impl FnOnce(Arc<T>) -> Signing) -> Self {
        Self {
            store: store.clone(),
            signing: signing(store),
        }
    }

    fn local_with(root: &Path, upload: Option<UploadSigner>) -> Result<Self, String> {
        std::fs::create_dir_all(root).map_err(|e| format!("create storage dir {}: {e}", root.display()))?;
        let store = LocalFileSystem::new_with_prefix(root).map_err(|e| e.to_string())?;
        Ok(Self {
            store: Arc::new(store),
            signing: Signing::Local {
                root: root.to_path_buf(),
                upload,
            },
        })
    }

    pub async fn put(&self, path: &str, data: Bytes) -> Result<(), String> {
        self.put_with(path, data, &ObjectMetadata::default()).await
    }

    pub async fn put_with(&self, path: &str, data: Bytes, metadata: &ObjectMetadata) -> Result<(), String> {
        let options = PutOptions {
            attributes: self.attributes(metadata),
            ..PutOptions::default()
        };
        self.store
            .put_opts(&ObjectPath::from(path), PutPayload::from(data), options)
            .await
            .map(|_| ())
            .map_err(|e| format!("put {path}: {e}"))
    }

    pub async fn put_file(&self, path: &str, file: &Path) -> Result<(), String> {
        self.put_file_with(path, file, &ObjectMetadata::default()).await
    }

    pub async fn put_file_with(&self, path: &str, file: &Path, metadata: &ObjectMetadata) -> Result<(), String> {
        let mut source = tokio::fs::File::open(file)
            .await
            .map_err(|e| format!("open {}: {e}", file.display()))?;
        let mut writer =
            BufWriter::new(self.store.clone(), ObjectPath::from(path)).with_attributes(self.attributes(metadata));
        tokio::io::copy(&mut source, &mut writer)
            .await
            .map_err(|e| format!("put {path}: {e}"))?;
        writer.shutdown().await.map_err(|e| format!("put {path}: {e}"))
    }

    fn attributes(&self, metadata: &ObjectMetadata) -> Attributes {
        match self.signing {
            Signing::Local { .. } => Attributes::default(),
            _ => metadata
                .entries
                .iter()
                .map(|(key, value)| {
                    (
                        Attribute::Metadata(key.clone().into()),
                        AttributeValue::from(value.clone()),
                    )
                })
                .collect(),
        }
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

    pub fn open_blob(&self, path: &str) -> Result<(), String> {
        match &self.signing {
            Signing::Local { root, .. } => {
                let parent = crate::go_os::parent(&root.join(path));
                std::fs::create_dir_all(&parent).map_err(|e| crate::go_os::path_error("mkdir", &parent, &e))
            }
            _ => Ok(()),
        }
    }

    pub async fn upload_signed_url(&self, path: &str, expires_in: Duration) -> Result<UploadUrl, String> {
        match &self.signing {
            Signing::Local { upload: None, .. } => {
                Err("file system storage does not support signed URLs (no local upload endpoint configured)".to_owned())
            }
            Signing::Local {
                upload: Some(signer), ..
            } => Ok(UploadUrl {
                url: signer.signed_url(path, expires_in),
                headers: HashMap::new(),
            }),
            Signing::S3(store) => sign(store.as_ref(), path, expires_in)
                .await
                .map_err(|e| format!("failed to presign PUT URL: {e}")),
            Signing::Gcs(store) => sign(store.as_ref(), path, expires_in)
                .await
                .map_err(|e| format!("failed to create signed URL for GCS object ({path}): {e}")),
            Signing::Azure(store) => sign(store.as_ref(), path, expires_in)
                .await
                .map(|url| UploadUrl {
                    headers: HashMap::from([(AZURE_BLOB_TYPE_HEADER.to_owned(), AZURE_BLOCK_BLOB.to_owned())]),
                    ..url
                })
                .map_err(|e| {
                    format!(
                        "failed to sign Azure upload SAS for {}: {e}",
                        crate::go_strconv::quote(path)
                    )
                }),
        }
    }

    pub async fn load_header(&self, path: &str) -> Result<Header, String> {
        Header::decode(&self.get(path).await?)
    }

    pub async fn store_header(&self, path: &str, header: &Header) -> Result<(), String> {
        self.put(path, Bytes::from(header.encode())).await
    }
}

async fn sign(signer: &dyn Signer, path: &str, expires_in: Duration) -> Result<UploadUrl, String> {
    signer
        .signed_url(Method::PUT, &ObjectPath::from(path), expires_in)
        .await
        .map(|url| UploadUrl {
            url: url.to_string(),
            headers: HashMap::new(),
        })
        .map_err(|e| e.to_string())
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
        let spec = StorageSpec {
            provider: Provider::Aws,
            bucket: "e2b-test".to_owned(),
            endpoint,
            use_path_style: true,
            region: "us-east-1".to_owned(),
            ..StorageSpec::local("")
        };
        let credentials = AmazonS3Builder::new()
            .with_access_key_id("minio")
            .with_secret_access_key("minio123");
        let storage = Storage::s3(&spec, credentials).unwrap();
        exercise(&storage, &Uuid::new_v4().to_string()).await;
    }
}
