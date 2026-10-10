pub mod aws;
pub mod azure;
mod blocking;
mod credential_cache;
pub mod gcs;
pub(crate) mod pem;
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
use object_store::buffered::BufWriter;
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
use aws::sse::ServerSideEncryption;
use azure::{AzureBackend, AzureEnv, AzureSigner};
use gcs::ServiceAccountSigner;

const AZURE_BLOB_TYPE_HEADER: &str = "x-ms-blob-type";
const AZURE_BLOCK_BLOB: &str = "BlockBlob";
const OBJECT_NOT_EXIST: &str = "object does not exist";
const EMPTY_PREFIX_REFUSED: &str = "refusing to delete objects with an empty prefix";
const RATE_LIMITED: &str = "object access rate limited";
const TOO_MANY_REQUESTS_STATUS: &str = "Server returned non-2xx status code: 429";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutError {
    RateLimited,
    Failed(String),
}

impl std::fmt::Display for PutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => f.write_str(RATE_LIMITED),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

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
    S3 {
        store: Arc<AmazonS3>,
        encryption: ServerSideEncryption,
    },
    Gcs(ServiceAccountSigner),
    Azure(AzureSigner),
}

impl Storage {
    pub fn local(root: &Path) -> Result<Self, String> {
        Self::local_with(root, None)
    }

    pub fn open(spec: &StorageSpec, upload: Option<UploadSigner>) -> Result<Self, String> {
        match spec.provider {
            Provider::Local => Self::local_with(Path::new(&spec.base_path), upload),
            Provider::Aws => Self::s3(spec, providers::default_s3_builder(spec)),
            Provider::Gcp => providers::gcs(spec).map(|store| Self::gcs(spec, store)),
            Provider::Azure => Self::azure(spec),
        }
    }

    fn azure(spec: &StorageSpec) -> Result<Self, String> {
        let env = |name: &str| std::env::var(name).ok();
        let connection_string = env("AZURE_STORAGE_CONNECTION_STRING");
        let account_name = env("AZURE_STORAGE_ACCOUNT_NAME");
        let account_key = env("AZURE_STORAGE_ACCOUNT_KEY");
        azure::open(
            spec,
            AzureEnv {
                connection_string: connection_string.as_deref(),
                account_name: account_name.as_deref(),
                account_key: account_key.as_deref(),
            },
        )
        .map(Self::from_azure)
    }

    fn from_azure(backend: AzureBackend) -> Self {
        Self::with_signing(backend.store, Signing::Azure(backend.signer))
    }

    pub fn s3(spec: &StorageSpec, builder: AmazonS3Builder) -> Result<Self, String> {
        providers::s3(spec, builder).map(|store| {
            Self::remote(Arc::new(store), |store| Signing::S3 {
                store,
                encryption: ServerSideEncryption::of(spec),
            })
        })
    }

    #[cfg(test)]
    pub fn open_azure_for_test() -> Self {
        let spec = StorageSpec {
            provider: Provider::Azure,
            bucket: "c".to_owned(),
            ..StorageSpec::local("")
        };
        azure::open(
            &spec,
            AzureEnv {
                connection_string: Some("AccountName=account;AccountKey=a2V5"),
                account_name: None,
                account_key: None,
            },
        )
        .map(Self::from_azure)
        .unwrap()
    }

    #[cfg(test)]
    pub fn gcs_for_test(store: impl ObjectStore) -> Self {
        let spec = StorageSpec {
            provider: Provider::Gcp,
            bucket: "bucket".to_owned(),
            ..StorageSpec::local("")
        };
        Self::gcs(&spec, store)
    }

    fn gcs(spec: &StorageSpec, store: impl ObjectStore) -> Self {
        let signer = ServiceAccountSigner::from_env(&spec.bucket);
        Self::with_signing(store, Signing::Gcs(signer))
    }

    fn with_signing(store: impl ObjectStore, signing: Signing) -> Self {
        Self {
            store: Arc::new(store),
            signing,
        }
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
        self.put_object(path, data, metadata).await.map_err(|e| e.to_string())
    }

    pub async fn put_object(&self, path: &str, data: Bytes, metadata: &ObjectMetadata) -> Result<(), PutError> {
        let options = PutOptions {
            attributes: self.attributes(metadata),
            ..PutOptions::default()
        };
        self.store
            .put_opts(&ObjectPath::from(path), PutPayload::from(data), options)
            .await
            .map(|_| ())
            .map_err(|e| self.put_error(path, &e))
    }

    fn put_error(&self, path: &str, error: &object_store::Error) -> PutError {
        match (&self.signing, is_rate_limited(error)) {
            (Signing::Gcs(_), true) => PutError::RateLimited,
            _ => PutError::Failed(format!("put {path}: {error}")),
        }
    }

    pub async fn put_file(&self, path: &str, file: &Path) -> Result<(), String> {
        self.put_file_with(path, file, &ObjectMetadata::default()).await
    }

    pub async fn put_file_with(&self, path: &str, file: &Path, metadata: &ObjectMetadata) -> Result<(), String> {
        let mut source = tokio::fs::File::open(file)
            .await
            .map_err(|e| format!("open {}: {e}", file.display()))?;
        let writer =
            BufWriter::new(self.store.clone(), ObjectPath::from(path)).with_attributes(self.attributes(metadata));
        upload(&mut source, writer, path).await
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

    pub async fn write_to(&self, path: &str, destination: &Path) -> Result<(), String> {
        let mut chunks = self.blob_stream(path).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(destination)
            .await
            .map_err(|e| crate::go_os::path_error("open", destination, &e))?;
        while let Some(chunk) = chunks.try_next().await? {
            file.write_all(&chunk)
                .await
                .map_err(|e| crate::go_os::path_error("write", destination, &e))?;
        }
        file.flush()
            .await
            .map_err(|e| crate::go_os::path_error("write", destination, &e))
    }

    async fn blob_stream(&self, path: &str) -> Result<BoxStream<'static, Result<Bytes, String>>, String> {
        if let Signing::Local { root, .. } = &self.signing {
            local_blob_handle(&root.join(path))?;
        }
        let object = self
            .store
            .get(&ObjectPath::from(path))
            .await
            .map_err(|e| self.reader_error(path, e))?;
        let copy_error = self.copy_error(path);
        Ok(object.into_stream().map_err(copy_error).boxed())
    }

    fn reader_error(&self, path: &str, error: object_store::Error) -> String {
        let reason = match error {
            object_store::Error::NotFound { .. } => OBJECT_NOT_EXIST.to_owned(),
            error => error.to_string(),
        };
        match self.signing {
            Signing::Gcs(_) => format!(
                "failed to create reader for {}: {reason}",
                crate::go_strconv::quote(path)
            ),
            _ => reason,
        }
    }

    fn copy_error(&self, path: &str) -> impl Fn(object_store::Error) -> String + Send + 'static {
        let gcs_path = matches!(self.signing, Signing::Gcs(_)).then(|| crate::go_strconv::quote(path));
        move |error| match &gcs_path {
            Some(quoted) => format!("failed to copy {quoted} to buffer: {error}"),
            None => error.to_string(),
        }
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
        match (&self.signing, prefix.is_empty()) {
            (Signing::S3 { .. } | Signing::Azure(_), true) => Err(EMPTY_PREFIX_REFUSED.to_owned()),
            (Signing::Gcs(_), true) => Ok(()),
            _ => self.delete_listed(prefix).await,
        }
    }

    async fn delete_listed(&self, prefix: &str) -> Result<(), String> {
        let prefix_path = ObjectPath::from(prefix);
        let locations = self
            .store
            .list(Some(&prefix_path))
            .map_ok(|object| object.location)
            .boxed();
        self.store
            .delete_stream(locations)
            .filter(|deleted| std::future::ready(!matches!(deleted, Err(object_store::Error::NotFound { .. }))))
            .try_for_each(|_| std::future::ready(Ok(())))
            .await
            .map_err(|e| format!("delete {prefix}: {e}"))
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
            Signing::S3 { store, encryption } => sign(store.as_ref(), encryption, path, expires_in)
                .await
                .map_err(|e| format!("failed to presign PUT URL: {e}")),
            Signing::Gcs(signer) => signer.upload_url(path, expires_in).map(|url| UploadUrl {
                url,
                headers: HashMap::new(),
            }),
            Signing::Azure(signer) => signer.upload_url(path, expires_in).await.map(|url| UploadUrl {
                url,
                headers: HashMap::from([(AZURE_BLOB_TYPE_HEADER.to_owned(), AZURE_BLOCK_BLOB.to_owned())]),
            }),
        }
    }

    pub async fn load_header(&self, path: &str) -> Result<Header, String> {
        Header::decode(&self.get(path).await?)
    }

    pub async fn store_header(&self, path: &str, header: &Header) -> Result<(), String> {
        self.put(path, Bytes::from(header.serialize()?)).await
    }
}

async fn upload(
    source: &mut (impl tokio::io::AsyncRead + Unpin),
    mut writer: BufWriter,
    path: &str,
) -> Result<(), String> {
    match tokio::io::copy(source, &mut writer).await {
        Ok(_) => writer.shutdown().await.map_err(|e| format!("put {path}: {e}")),
        Err(e) => {
            writer.abort().await.ok();
            Err(format!("put {path}: {e}"))
        }
    }
}

fn is_rate_limited(error: &object_store::Error) -> bool {
    let root: &(dyn std::error::Error + 'static) = error;
    std::iter::successors(Some(root), |cause| cause.source())
        .any(|cause| cause.to_string().starts_with(TOO_MANY_REQUESTS_STATUS))
}

async fn sign(
    signer: &dyn Signer,
    encryption: &ServerSideEncryption,
    path: &str,
    expires_in: Duration,
) -> Result<UploadUrl, String> {
    signer
        .signed_url_opts(
            Method::PUT,
            &ObjectPath::from(path),
            expires_in,
            &encryption.signed_url_options()?,
        )
        .await
        .map(|url| UploadUrl {
            url: url.to_string(),
            headers: encryption.headers(),
        })
        .map_err(|e| e.to_string())
}

fn local_blob_handle(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(info) if info.is_dir() => Err(format!("path {} is a directory", path.display())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(OBJECT_NOT_EXIST.to_owned()),
        Err(e) => Err(crate::go_os::path_error("stat", path, &e)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

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
    async fn write_to_copies_a_blob_and_reports_missing_ones_like_go() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        storage.put("s/files/h.tar", Bytes::from_static(b"tar")).await.unwrap();
        let destination = dir.path().join("layer.tar");
        std::fs::write(&destination, b"").unwrap();

        storage.write_to("s/files/h.tar", &destination).await.unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"tar");
        assert_eq!(
            storage.write_to("s/files/missing.tar", &destination).await,
            Err("object does not exist".to_owned())
        );
        assert_eq!(
            storage.write_to("s/files", &destination).await,
            Err(format!(
                "path {} is a directory",
                dir.path().join("store/s/files").display()
            ))
        );
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
    async fn s3_round_trip_when_an_s3_endpoint_is_available() {
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
            .with_access_key_id("test-access-key")
            .with_secret_access_key("test-secret-key");
        let storage = Storage::s3(&spec, credentials).unwrap();
        exercise(&storage, &Uuid::new_v4().to_string()).await;
    }

    #[tokio::test]
    async fn a_rate_limited_gcs_put_reports_the_go_rate_limit_error() {
        let storage = crate::storage::testing::rate_limited_gcs().await;

        assert_eq!(
            storage
                .put_object("i/h", Bytes::from_static(b"x"), &ObjectMetadata::default())
                .await,
            Err(PutError::RateLimited)
        );
        assert_eq!(
            storage.put("i/h", Bytes::from_static(b"x")).await,
            Err("object access rate limited".to_owned())
        );
    }

    #[tokio::test]
    async fn an_empty_prefix_is_refused_on_s3_and_azure_ignored_on_gcs_and_wipes_local() {
        let spec = StorageSpec {
            provider: Provider::Aws,
            bucket: "b".to_owned(),
            region: "us-east-1".to_owned(),
            ..StorageSpec::local("")
        };
        let credentials = AmazonS3Builder::new()
            .with_access_key_id("test-access-key")
            .with_secret_access_key("test-secret-key");
        let s3 = Storage::s3(&spec, credentials).unwrap();
        let azure = Storage::open_azure_for_test();
        let gcs = crate::storage::testing::rate_limited_gcs().await;
        let dir = tempfile::tempdir().unwrap();
        let local = Storage::local(dir.path()).unwrap();
        local.put("a/one", Bytes::from_static(b"1")).await.unwrap();
        local.put("b/two", Bytes::from_static(b"2")).await.unwrap();

        let refused = Err("refusing to delete objects with an empty prefix".to_owned());
        assert_eq!(s3.delete_prefix("").await, refused);
        assert_eq!(azure.delete_prefix("").await, refused);
        assert_eq!(gcs.delete_prefix("").await, Ok(()));
        assert_eq!(local.delete_prefix("").await, Ok(()));
        assert_eq!(local.exists("a/one").await, Ok(false));
        assert_eq!(local.exists("b/two").await, Ok(false));
    }

    #[tokio::test]
    async fn delete_prefix_removes_only_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        storage.put("a/one", Bytes::from_static(b"1")).await.unwrap();
        storage.put("a/deep/two", Bytes::from_static(b"2")).await.unwrap();
        storage.put("ab/three", Bytes::from_static(b"3")).await.unwrap();

        storage.delete_prefix("a").await.unwrap();

        assert_eq!(storage.exists("a/one").await, Ok(false));
        assert_eq!(storage.exists("a/deep/two").await, Ok(false));
        assert_eq!(storage.exists("ab/three").await, Ok(true));
        assert_eq!(storage.delete_prefix("missing").await, Ok(()));
    }

    #[tokio::test]
    async fn put_file_uploads_large_files_in_parts() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let content: Vec<u8> = (0..25 << 20).map(|i: u32| (i % 251) as u8).collect();
        let source = dir.path().join("memfile");
        std::fs::write(&source, &content).unwrap();

        storage.put_file("t/memfile", &source).await.unwrap();

        assert_eq!(storage.get("t/memfile").await.unwrap(), Bytes::from(content));
    }

    #[tokio::test]
    async fn a_failed_part_aborts_the_multipart_upload() {
        let aborted = Arc::new(AtomicBool::new(false));
        let store: Arc<dyn ObjectStore> = Arc::new(crate::storage::testing::FailingParts::new(aborted.clone()));
        let writer = BufWriter::with_capacity(store, ObjectPath::from("t/memfile"), 1 << 20).with_max_concurrency(1);
        let mut source = std::io::Cursor::new(vec![7u8; 8 << 20]);

        let result = upload(&mut source, writer, "t/memfile").await;

        assert!(result.unwrap_err().starts_with("put t/memfile: "));
        assert!(aborted.load(Ordering::SeqCst));
    }
}
