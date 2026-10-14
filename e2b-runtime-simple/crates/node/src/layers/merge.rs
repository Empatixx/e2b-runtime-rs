use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use overlaybd::backend::local::LocalFile;
use overlaybd::backend::switch::new_switch_file;
use overlaybd::backend::tar::new_tar_file_adaptor;
use overlaybd::index_file::{CommitArgs, merge_files_ro};
use overlaybd::virtual_file::VirtualFile;

const MERGE_CONCURRENCY: usize = 32;

pub async fn merge_layers(sources: &[PathBuf], output: &Path) -> Result<()> {
    let mut files = Vec::with_capacity(sources.len());
    for source in sources {
        files.push(open_layer(source).await?);
    }
    let tmp = output.with_extension("merge.tmp");
    let target: Arc<dyn VirtualFile> = Arc::new(LocalFile::new(&tmp)?);
    let commit = CommitArgs {
        concurrency: MERGE_CONCURRENCY,
        ..CommitArgs::new(target)
    };
    let merged = merge_files_ro(&files, commit).await.context("merge layers");
    if merged.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    merged?;
    tokio::fs::rename(&tmp, output).await?;
    Ok(())
}

async fn open_layer(path: &Path) -> Result<Arc<dyn VirtualFile>> {
    let local: Arc<dyn VirtualFile> =
        Arc::new(LocalFile::open_ro(path).with_context(|| format!("open layer {}", path.display()))?);
    let adapted = new_tar_file_adaptor(local).await?;
    let switched = new_switch_file(adapted, true, Some(&path.display().to_string())).await?;
    Ok(switched)
}
