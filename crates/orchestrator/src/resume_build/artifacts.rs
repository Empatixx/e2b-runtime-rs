use std::os::unix::fs::MetadataExt;
use std::path::Path;

use uuid::Uuid;

use crate::storage::header::Header;
use crate::storage::paths::BuildPaths;
use crate::storage::spec::{Provider, StorageSpec};

struct MainArtifact {
    name: &'static str,
    file: String,
    header: String,
}

pub fn print_artifact_sizes(spec: &StorageSpec, build_id: &str) {
    let Ok(id) = Uuid::parse_str(build_id) else {
        return;
    };
    if spec.provider != Provider::Local {
        return;
    }
    let base = Path::new(&spec.base_path);
    let paths = BuildPaths::new(id);
    println!("\n📦 Artifacts:");
    [
        MainArtifact {
            name: "Rootfs",
            file: paths.rootfs(),
            header: paths.rootfs_header(),
        },
        MainArtifact {
            name: "Memfile",
            file: paths.memfile(),
            header: paths.memfile_header(),
        },
    ]
    .iter()
    .for_each(|artifact| print_main(base, artifact));
    [
        ("Rootfs header", paths.rootfs_header()),
        ("Memfile header", paths.memfile_header()),
        ("Snapfile", paths.snapfile()),
        ("Metadata", paths.metadata()),
    ]
    .iter()
    .filter_map(|(name, file)| on_disk(&base.join(file)).map(|size| (name, size)))
    .for_each(|(name, size)| println!("   {name}: {} KB", size >> 10));
}

fn print_main(base: &Path, artifact: &MainArtifact) {
    let Some(actual) = on_disk(&base.join(&artifact.file)) else {
        return;
    };
    match header_info(&base.join(&artifact.header)) {
        None => println!("   {}: {} MB (this layer)", artifact.name, actual >> 20),
        Some((total, block_size)) => println!(
            "   {}: {} MB diff / {} MB total ({:.1}%), block size: {} KB",
            artifact.name,
            actual >> 20,
            total >> 20,
            actual as f64 / total as f64 * 100.0,
            block_size >> 10
        ),
    }
}

fn on_disk(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|metadata| metadata.blocks() * 512)
}

fn header_info(path: &Path) -> Option<(u64, u64)> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| Header::decode(&bytes).ok())
        .map(|header| (header.metadata.size, header.metadata.block_size))
        .filter(|(total, _)| *total > 0)
}
