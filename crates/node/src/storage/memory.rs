use std::io::IoSliceMut;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use bytes::Bytes;
use nix::sys::uio::{RemoteIoVec, process_vm_readv};
use nix::unistd::Pid;
use overlaybd::backend::local::LocalFile;
use overlaybd::index::{Segment, SegmentMapping};
use overlaybd::index_file::{CommitArgs, compact_to};
use overlaybd::virtual_file::VirtualFile;
use serde::Deserialize;

const PAGE: u64 = 4096;
const SECTOR: u64 = 512;
const COMPACT_CONCURRENCY: usize = 32;

#[derive(Debug, Deserialize)]
pub struct DirtyMemory {
    pub page_size: u64,
    pub memory_size: u64,
    pub ranges: Vec<DirtyRange>,
}

#[derive(Debug, Deserialize)]
pub struct DirtyRange {
    pub base_host_virt_addr: u64,
    pub image_offset: u64,
    pub length: u64,
}

pub async fn write_memory_layer(pid: u32, dirty: &DirtyMemory, layer: &Path) -> Result<()> {
    ensure!(
        dirty.page_size == PAGE,
        "unexpected dirty page size {}",
        dirty.page_size
    );
    let mappings = segment_mappings(&dirty.ranges);
    let source: Arc<dyn VirtualFile> = Arc::new(GuestMemory {
        pid: Pid::from_raw(pid as i32),
    });
    let tmp = layer.with_extension("commit.tmp");
    let output: Arc<dyn VirtualFile> = Arc::new(LocalFile::new(&tmp)?);
    let commit = CommitArgs {
        concurrency: COMPACT_CONCURRENCY,
        ..CommitArgs::new(output)
    };
    compact_to(&[source], &mappings, dirty.memory_size, commit)
        .await
        .context("compact guest memory into a layer")?;
    tokio::fs::rename(&tmp, layer).await?;
    Ok(())
}

fn segment_mappings(ranges: &[DirtyRange]) -> Vec<SegmentMapping> {
    let mut mappings: Vec<SegmentMapping> = ranges.iter().flat_map(range_segments).collect();
    mappings.sort_unstable();
    mappings
}

fn range_segments(range: &DirtyRange) -> impl Iterator<Item = SegmentMapping> + '_ {
    let max_sectors = Segment::MAX_LENGTH as u64;
    let sectors = range.length / SECTOR;
    (0..sectors.div_ceil(max_sectors)).map(move |chunk| {
        let start = chunk * max_sectors;
        SegmentMapping::new(
            range.image_offset / SECTOR + start,
            (sectors - start).min(max_sectors) as u32,
            range.base_host_virt_addr / SECTOR + start,
            false,
            0,
        )
    })
}

struct GuestMemory {
    pid: Pid,
}

impl GuestMemory {
    fn read_exact(&self, address: u64, mut dst: &mut [u8]) -> Result<()> {
        let mut address = address;
        while !dst.is_empty() {
            let remote = [RemoteIoVec {
                base: address as usize,
                len: dst.len(),
            }];
            let read = match process_vm_readv(self.pid, &mut [IoSliceMut::new(dst)], &remote) {
                Ok(0) => bail!("process_vm_readv returned 0 at {address:#x}"),
                Ok(read) => read,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e).context(format!("process_vm_readv at {address:#x}")),
            };
            address += read as u64;
            dst = &mut dst[read..];
        }
        Ok(())
    }
}

#[async_trait]
impl VirtualFile for GuestMemory {
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        let mut data = vec![0u8; len];
        self.read_exact(offset, &mut data)?;
        Ok(Bytes::from(data))
    }

    async fn read_at_into(&self, offset: u64, dst: &mut [u8]) -> Result<usize> {
        self.read_exact(offset, dst)?;
        Ok(dst.len())
    }

    async fn write_at(&self, _offset: u64, _data: &[u8]) -> Result<usize> {
        bail!("guest memory is read only")
    }

    async fn size(&self) -> Result<u64> {
        bail!("guest memory has no size")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_ranges_into_segments() {
        let max = Segment::MAX_LENGTH as u64 * SECTOR;
        let range = DirtyRange {
            base_host_virt_addr: 0x10000,
            image_offset: 0,
            length: max + PAGE,
        };
        let segments: Vec<_> = range_segments(&range).collect();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].offset(), Segment::MAX_LENGTH as u64);
        assert_eq!(u64::from(segments[1].length()), PAGE / SECTOR);
    }

    #[tokio::test]
    async fn reads_own_memory() {
        let source = b"guest-memory";
        let memory = GuestMemory { pid: Pid::this() };
        let bytes = memory.read_at(source.as_ptr() as u64, source.len()).await.unwrap();
        assert_eq!(&bytes[..], source);
    }
}
