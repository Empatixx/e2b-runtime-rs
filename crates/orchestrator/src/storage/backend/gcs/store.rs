use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;

use futures::stream::BoxStream;
use object_store::gcp::GoogleCloudStorage;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions,
    PutOptions, PutPayload, PutResult,
};

type Attempt<'a, T> = Pin<Box<dyn Future<Output = object_store::Result<T>> + Send + 'a>>;

pub struct GcsStore {
    default_path: GoogleCloudStorage,
    multipart_path: GoogleCloudStorage,
}

impl GcsStore {
    pub fn new(default_path: GoogleCloudStorage, multipart_path: GoogleCloudStorage) -> Self {
        Self {
            default_path,
            multipart_path,
        }
    }
}

impl Display for GcsStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.default_path, f)
    }
}

impl Debug for GcsStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcsStore").finish_non_exhaustive()
    }
}

impl ObjectStore for GcsStore {
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
        Box::pin(self.default_path.put_opts(location, payload, opts))
    }

    fn put_multipart_opts<'a, 'b, 'c>(
        &'a self,
        location: &'b Path,
        opts: PutMultipartOptions,
    ) -> Attempt<'c, Box<dyn MultipartUpload>>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(self.multipart_path.put_multipart_opts(location, opts))
    }

    fn get_opts<'a, 'b, 'c>(&'a self, location: &'b Path, options: GetOptions) -> Attempt<'c, GetResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(self.default_path.get_opts(location, options))
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.default_path.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.default_path.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.default_path.list_with_offset(prefix, offset)
    }

    fn list_with_delimiter<'a, 'b, 'c>(&'a self, prefix: Option<&'b Path>) -> Attempt<'c, ListResult>
    where
        'a: 'c,
        'b: 'c,
        Self: 'c,
    {
        Box::pin(self.default_path.list_with_delimiter(prefix))
    }

    fn copy_opts<'a, 'b, 'c, 'd>(&'a self, from: &'b Path, to: &'c Path, options: CopyOptions) -> Attempt<'d, ()>
    where
        'a: 'd,
        'b: 'd,
        'c: 'd,
        Self: 'd,
    {
        Box::pin(self.default_path.copy_opts(from, to, options))
    }
}
