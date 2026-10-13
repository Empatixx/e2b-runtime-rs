use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flate2::read::GzDecoder;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio_util::io::{StreamReader, SyncIoBridge};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::pull::PulledImage;
use super::registry::{Descriptor, RegistrySession};
use super::untar::untar;
use crate::builder::logs::ScopedLog;
use crate::proto::LogLevel;
use bytesize::ByteSize;

const LAYER_DIR_MODE: u32 = 0o755;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

pub enum LayerSource {
    Bytes(Vec<u8>),
    Remote {
        session: Arc<RegistrySession>,
        descriptor: Descriptor,
    },
}

pub struct ImageLayer {
    pub digest: String,
    pub size: u64,
    pub source: LayerSource,
}

pub fn remote_layers(image: &PulledImage) -> Vec<ImageLayer> {
    image
        .layers()
        .iter()
        .map(|layer| ImageLayer {
            digest: layer.digest.clone(),
            size: layer.size,
            source: LayerSource::Remote {
                session: image.session.clone(),
                descriptor: layer.clone(),
            },
        })
        .collect()
}

pub async fn create_export(
    log: &ScopedLog<'_>,
    layers: Vec<ImageLayer>,
    path: &Path,
    cancel: &CancellationToken,
    tracker: &TaskTracker,
) -> Result<Vec<PathBuf>, String> {
    let (paths, tasks): (Vec<PathBuf>, FuturesUnordered<_>) = layers
        .into_iter()
        .enumerate()
        .map(|(index, layer)| {
            log.push(
                LogLevel::Info,
                format!(
                    "Uncompressing layer {} {}",
                    layer.digest,
                    ByteSize::b(layer.size).display().si()
                ),
            );
            let layer_path = path.join(format!("layer-{index}-{}", layer.digest.replace(':', "-")));
            let task = tracker.spawn(extract(index, layer.source, layer_path.clone(), cancel.clone()));
            (layer_path, task)
        })
        .unzip();
    let first_error = tasks
        .map(|joined| joined.map_err(|e| e.to_string()).and_then(|outcome| outcome))
        .fold(None, |first, outcome| async move { first.or(outcome.err()) })
        .await;
    if let Some(e) = first_error {
        return Err(format!("while extracting layers: {e}"));
    }
    log.push(LogLevel::Info, "Layers extracted");
    Ok(paths.into_iter().rev().collect())
}

async fn extract(index: usize, source: LayerSource, target: PathBuf, cancel: CancellationToken) -> Result<(), String> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(LAYER_DIR_MODE)
        .create(&target)
        .await
        .map_err(|e| format!("failed to create directory for layer {index}: {e}"))?;
    let raw = open(source)
        .await
        .map_err(|e| format!("failed to get uncompressed layer {index}: {e}"))?;
    let raw: Box<dyn Read + Send> = Box::new(Cancellable { inner: raw, cancel });
    tokio::task::spawn_blocking(move || {
        let reader = uncompressed(raw).map_err(|e| format!("failed to get uncompressed layer {index}: {e}"))?;
        untar(reader, &target).map_err(|e| format!("failed to untar layer {index}: {e}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn open(source: LayerSource) -> io::Result<Box<dyn Read + Send>> {
    match source {
        LayerSource::Bytes(bytes) => Ok(Box::new(io::Cursor::new(bytes))),
        LayerSource::Remote { session, descriptor } => session
            .blob_stream(&descriptor)
            .await
            .map(|stream| Box::new(SyncIoBridge::new(StreamReader::new(stream))) as Box<dyn Read + Send>)
            .map_err(|e| io::Error::other(e.to_string())),
    }
}

struct Cancellable {
    inner: Box<dyn Read + Send>,
    cancel: CancellationToken,
}

impl Read for Cancellable {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.cancel.is_cancelled() {
            true => Err(io::Error::other("context canceled")),
            false => self.inner.read(buffer),
        }
    }
}

fn uncompressed(mut raw: Box<dyn Read + Send>) -> io::Result<Box<dyn Read + Send>> {
    let mut magic = [0u8; 4];
    let read = read_prefix(&mut raw, &mut magic)?;
    let prefixed: Box<dyn Read + Send> = Box::new(io::Cursor::new(magic[..read].to_vec()).chain(raw));
    match &magic[..read] {
        [a, b, ..] if [*a, *b] == GZIP_MAGIC => Ok(Box::new(GzDecoder::new(prefixed))),
        prefix if prefix == ZSTD_MAGIC => Ok(Box::new(zstd::stream::read::Decoder::new(prefixed)?)),
        _ => Ok(prefixed),
    }
}

fn read_prefix(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};

    use super::*;
    use crate::builder::logs::BuildLog;

    fn layer(index: usize, compress: bool) -> ImageLayer {
        let mut builder = Builder::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_mode(0o600);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(10);
        builder
            .append_data(&mut header, format!("layer{index}.txt"), b"layer text".as_slice())
            .unwrap();
        let tar = builder.into_inner().unwrap();
        let bytes = match compress {
            true => {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(&tar).unwrap();
                encoder.finish().unwrap()
            }
            false => tar,
        };
        ImageLayer {
            digest: format!("sha256:{index}{index}"),
            size: bytes.len() as u64,
            source: LayerSource::Bytes(bytes),
        }
    }

    #[tokio::test]
    async fn layers_come_back_top_first() {
        if !nix::unistd::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let log = BuildLog::default();
        let fields = Default::default();

        let paths = create_export(
            &log.scoped(&fields),
            vec![layer(0, true), layer(1, false), layer(2, true)],
            dir.path(),
            &CancellationToken::new(),
            &TaskTracker::new(),
        )
        .await
        .unwrap();

        assert!(paths[0].ends_with("layer-2-sha256-22"));
        assert!(paths[0].join("layer2.txt").is_file());
        assert!(paths[2].ends_with("layer-0-sha256-00"));
        let lines: Vec<String> = log
            .query(&Default::default())
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        assert_eq!(
            lines
                .first()
                .map(|line| line.starts_with("Uncompressing layer sha256:00 ")),
            Some(true)
        );
        assert_eq!(lines.last().map(String::as_str), Some("Layers extracted"));
    }
}
