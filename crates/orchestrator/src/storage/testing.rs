use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::stream::BoxStream;
use object_store::gcp::{GcpCredential, GoogleCloudStorageBuilder};
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    ClientOptions, CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, RetryConfig, StaticCredentialProvider, UploadPart,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::backend::Storage;

const TOO_MANY_REQUESTS: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

pub async fn rate_limited_gcs() -> Storage {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (connection, _) = listener.accept().await.unwrap();
            tokio::spawn(refuse(connection));
        }
    });
    let store = GoogleCloudStorageBuilder::new()
        .with_bucket_name("bucket")
        .with_base_url(&format!("http://{address}"))
        .with_credentials(Arc::new(StaticCredentialProvider::new(GcpCredential {
            bearer: "token".to_owned(),
        })))
        .with_client_options(ClientOptions::new().with_allow_http(true))
        .with_retry(RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        })
        .build()
        .unwrap();
    Storage::gcs_for_test(store)
}

async fn refuse(mut connection: TcpStream) {
    let mut request = [0u8; 4096];
    connection.read(&mut request).await.map(drop).ok();
    connection.write_all(TOO_MANY_REQUESTS).await.ok();
    connection.shutdown().await.ok();
}

pub struct FailingParts {
    inner: InMemory,
    aborted: Arc<AtomicBool>,
}

impl FailingParts {
    pub fn new(aborted: Arc<AtomicBool>) -> Self {
        Self {
            inner: InMemory::new(),
            aborted,
        }
    }
}

impl Display for FailingParts {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("FailingParts")
    }
}

impl Debug for FailingParts {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("FailingParts")
    }
}

type Attempt<'a, T> = Pin<Box<dyn Future<Output = object_store::Result<T>> + Send + 'a>>;

#[derive(Debug)]
struct FailingUpload {
    aborted: Arc<AtomicBool>,
}

impl MultipartUpload for FailingUpload {
    fn put_part(&mut self, _data: PutPayload) -> UploadPart {
        Box::pin(async {
            Err(object_store::Error::Generic {
                store: "FailingParts",
                source: "part refused".into(),
            })
        })
    }

    fn complete<'a, 'b>(&'a mut self) -> Attempt<'b, PutResult>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async {
            Ok(PutResult {
                e_tag: None,
                version: None,
                extensions: Default::default(),
            })
        })
    }

    fn abort<'a, 'b>(&'a mut self) -> Attempt<'b, ()>
    where
        'a: 'b,
        Self: 'b,
    {
        Box::pin(async {
            self.aborted.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

impl ObjectStore for FailingParts {
    fn put_opts<'a, 'b, 'c>(
        &'a self,
        location: &'b Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Attempt<'c, PutResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(self.inner.put_opts(location, payload, opts))
    }

    fn put_multipart_opts<'a, 'b, 'c>(
        &'a self,
        _location: &'b Path,
        _opts: PutMultipartOptions,
    ) -> Attempt<'c, Box<dyn MultipartUpload>>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        let upload: Box<dyn MultipartUpload> = Box::new(FailingUpload {
            aborted: self.aborted.clone(),
        });
        Box::pin(async { Ok(upload) })
    }

    fn get_opts<'a, 'b, 'c>(&'a self, location: &'b Path, options: GetOptions) -> Attempt<'c, GetResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(self.inner.get_opts(location, options))
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_delimiter<'a, 'b, 'c>(&'a self, prefix: Option<&'b Path>) -> Attempt<'c, ListResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(self.inner.list_with_delimiter(prefix))
    }

    fn copy_opts<'a, 'b, 'c, 'd>(&'a self, from: &'b Path, to: &'c Path, options: CopyOptions) -> Attempt<'d, ()>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        Box::pin(self.inner.copy_opts(from, to, options))
    }
}
