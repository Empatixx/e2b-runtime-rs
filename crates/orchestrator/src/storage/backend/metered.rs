use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use bytes::Bytes;
use futures::stream::BoxStream;

use super::{Signing, Storage};
use crate::storage::frame_table::CompressionType;
use crate::telemetry::reads::{Outcome, SeekableObjectType, Source, reads};

pub struct OpenedRange {
    pub chunks: BoxStream<'static, Result<Bytes, String>>,
    pub source: Source,
}

impl Storage {
    pub(crate) fn source(&self) -> Source {
        match self.signing {
            Signing::Local { .. } => Source::Fs,
            Signing::S3 { .. } => Source::Aws,
            Signing::Gcs(_) => Source::Gcs,
            Signing::Azure(_) => Source::Azure,
        }
    }

    pub(crate) async fn get_direct(&self, path: &str) -> Result<Bytes, String> {
        let started = Instant::now();
        let bytes = self.get_raw(path).await;
        let transferred = bytes.as_ref().map(|bytes| bytes.len() as i64).map_err(String::clone);
        self.record_blob(path, started, &transferred);
        bytes
    }

    pub(crate) async fn find_direct(&self, path: &str) -> Result<Option<Bytes>, String> {
        let started = Instant::now();
        let found = self.find_raw(path).await;
        let transferred = found
            .as_ref()
            .map(|bytes| bytes.as_ref().map_or(0, |bytes| bytes.len() as i64))
            .map_err(String::clone);
        match &found {
            Ok(None) => reads().record_blob(started.elapsed(), 0, path, self.source(), Outcome::NotFound),
            _ => self.record_blob(path, started, &transferred),
        }
        found
    }

    pub(crate) async fn write_to_direct(&self, path: &str, destination: &Path) -> Result<(), String> {
        let started = Instant::now();
        let written = self.write_to_raw(path, destination).await;
        let transferred = written
            .as_ref()
            .map(|()| std::fs::metadata(destination).map_or(0, |metadata| metadata.len() as i64))
            .map_err(String::clone);
        self.record_blob(path, started, &transferred);
        written
    }

    pub(crate) async fn size_direct(&self, path: &str) -> Result<u64, String> {
        let started = Instant::now();
        let size = self.size_raw(path).await;
        reads().record_size(
            started.elapsed(),
            SeekableObjectType::of_path(path),
            self.source(),
            outcome_of(&size),
        );
        size
    }

    pub(crate) async fn open_range_direct(&self, path: &str, range: Range<u64>) -> Result<OpenedRange, String> {
        let started = Instant::now();
        let chunks = self.get_stream_raw(path, range).await;
        reads().record_open(
            started.elapsed(),
            SeekableObjectType::of_path(path),
            self.source(),
            CompressionType::None,
            outcome_of(&chunks),
        );
        chunks.map(|chunks| OpenedRange {
            chunks,
            source: self.source(),
        })
    }

    fn record_blob(&self, path: &str, started: Instant, transferred: &Result<i64, String>) {
        let (bytes, outcome) = match transferred {
            Ok(bytes) => (*bytes, Outcome::Ok),
            Err(error) => (0, Outcome::of_error(error)),
        };
        reads().record_blob(started.elapsed(), bytes, path, self.source(), outcome);
    }
}

fn outcome_of<T>(result: &Result<T, String>) -> Outcome {
    match result {
        Ok(_) => Outcome::Ok,
        Err(error) => Outcome::of_error(error),
    }
}
