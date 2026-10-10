use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
use super::spec::{Provider, StorageSpec};

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

pub type RecordedRequests = Arc<Mutex<HashMap<String, HashMap<String, String>>>>;

pub async fn recording_s3(spec: StorageSpec) -> (Storage, RecordedRequests) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = RecordedRequests::default();
    let seen = recorded.clone();
    tokio::spawn(async move {
        loop {
            let (connection, _) = listener.accept().await.unwrap();
            tokio::spawn(answer_s3(connection, seen.clone()));
        }
    });
    let spec = StorageSpec {
        provider: Provider::Aws,
        bucket: "test-bucket".to_owned(),
        endpoint: format!("http://{address}"),
        use_path_style: true,
        region: "us-east-1".to_owned(),
        ..spec
    };
    let credentials = object_store::aws::AmazonS3Builder::new()
        .with_access_key_id("test-access-key")
        .with_secret_access_key("test-secret-key");
    (Storage::s3(&spec, credentials).unwrap(), recorded)
}

async fn answer_s3(mut connection: TcpStream, recorded: RecordedRequests) {
    let mut received = Vec::new();
    let mut chunk = [0u8; 65536];
    let (head_end, content_length) = loop {
        let read = connection.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        received.extend_from_slice(&chunk[..read]);
        if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&received[..position]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (position + 4, length);
        }
    };
    while received.len() < head_end + content_length {
        let read = connection.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        received.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect::<HashMap<_, _>>();
    let operation = s3_operation(&request_line);
    recorded.lock().unwrap().insert(operation.to_owned(), headers);
    let (extra_headers, body) = s3_response(operation);
    let response = format!(
        "HTTP/1.1 200 OK\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    connection.write_all(response.as_bytes()).await.ok();
    connection.shutdown().await.ok();
}

fn s3_operation(request_line: &str) -> &'static str {
    let mut parts = request_line.split(' ');
    let (method, target) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    match method {
        "POST" if target.contains("uploads") => "CreateMultipartUpload",
        "POST" if target.contains("uploadId=") => "CompleteMultipartUpload",
        "PUT" if target.contains("partNumber=") => "UploadPart",
        "PUT" => "PutObject",
        _ => "Other",
    }
}

fn s3_response(operation: &str) -> (&'static str, &'static str) {
    match operation {
        "CreateMultipartUpload" => (
            "",
            "<InitiateMultipartUploadResult><UploadId>sse-upload-id</UploadId></InitiateMultipartUploadResult>",
        ),
        "CompleteMultipartUpload" => (
            "",
            "<CompleteMultipartUploadResult><Bucket>test-bucket</Bucket><Key>test-object</Key><ETag>\"complete-etag\"</ETag></CompleteMultipartUploadResult>",
        ),
        _ => ("etag: \"etag\"\r\n", ""),
    }
}
