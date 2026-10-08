use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

use super::untar::untar;
use crate::builder::logs::ScopedLog;
use crate::go_humanize::bytes;
use crate::proto::LogLevel;

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLayer {
    pub digest: String,
    pub size: u64,
    pub source: LayerSource,
}

pub async fn create_export(log: &ScopedLog<'_>, layers: Vec<ImageLayer>, path: &Path) -> Result<Vec<PathBuf>, String> {
    let count = layers.len();
    let tasks: Vec<_> = layers
        .into_iter()
        .enumerate()
        .map(|(index, layer)| {
            log.push(
                LogLevel::Info,
                format!("Uncompressing layer {} {}", layer.digest, bytes(layer.size)),
            );
            let layer_path = path.join(format!("layer-{index}-{}", layer.digest.replace(':', "-")));
            let target = layer_path.clone();
            let task = tokio::task::spawn_blocking(move || extract(index, &layer.source, &target));
            (layer_path, task)
        })
        .collect();
    let mut paths = Vec::with_capacity(count);
    let mut first_error = None;
    for (layer_path, task) in tasks {
        let outcome = task.await.map_err(|e| e.to_string()).and_then(|outcome| outcome);
        if let (Err(e), None) = (outcome, &first_error) {
            first_error = Some(e);
        }
        paths.push(layer_path);
    }
    if let Some(e) = first_error {
        return Err(format!("while extracting layers: {e}"));
    }
    log.push(LogLevel::Info, "Layers extracted");
    paths.reverse();
    Ok(paths)
}

fn extract(index: usize, source: &LayerSource, target: &Path) -> Result<(), String> {
    std::fs::create_dir_all(target).map_err(|e| format!("failed to create directory for layer {index}: {e}"))?;
    let reader = uncompressed(source).map_err(|e| format!("failed to get uncompressed layer {index}: {e}"))?;
    untar(reader, target).map_err(|e| format!("failed to untar layer {index}: {e}"))
}

fn uncompressed(source: &LayerSource) -> io::Result<Box<dyn Read + Send>> {
    let mut raw: Box<dyn Read + Send> = match source {
        LayerSource::File(path) => Box::new(BufReader::new(File::open(path)?)),
        LayerSource::Bytes(bytes) => Box::new(io::Cursor::new(bytes.clone())),
    };
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
