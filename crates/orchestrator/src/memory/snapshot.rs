use std::fs::File;
use std::io::{self, IoSliceMut, Write};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use nix::sys::uio::{RemoteIoVec, process_vm_readv};
use nix::unistd::Pid;
use roaring::RoaringBitmap;

use crate::firecracker::memory_info::GuestRegion;
use crate::storage::header::DiffMetadata;

const MAX_RUN_BYTES: u64 = 4 << 20;

struct Run {
    address: u64,
    length: u64,
}

pub fn memfile_pages(regions: &[GuestRegion], indices: &RoaringBitmap) -> RoaringBitmap {
    let pages: Vec<u32> = regions
        .iter()
        .flat_map(|region| {
            let first = region.offset / region.page_size;
            (first..first + region.size / region.page_size).map(|page| page as u32)
        })
        .collect();
    indices
        .iter()
        .filter_map(|index| pages.get(index as usize).copied())
        .collect()
}

pub enum GuestMemory {
    Process { pid: u32 },
    Memfd(Arc<File>),
}

impl GuestMemory {
    fn copy(
        &self,
        regions: &[GuestRegion],
        pages: &RoaringBitmap,
        page_size: u64,
        out: &mut impl Write,
    ) -> io::Result<()> {
        match self {
            GuestMemory::Process { pid } => runs(regions, pages, page_size)?
                .iter()
                .try_for_each(|run| copy_run(*pid, run, out)),
            GuestMemory::Memfd(memfd) => offset_runs(pages, page_size)
                .iter()
                .try_for_each(|run| copy_memfd_run(memfd, run, out)),
        }
    }
}

pub fn export_pages(
    memory: &GuestMemory,
    regions: &[GuestRegion],
    dirty: &RoaringBitmap,
    empty: &RoaringBitmap,
    out: &mut impl Write,
) -> io::Result<DiffMetadata> {
    let page_size = regions
        .first()
        .map(|region| region.page_size)
        .ok_or_else(|| io::Error::other("firecracker reported no memory regions"))?;
    memory.copy(regions, dirty, page_size, out)?;
    Ok(DiffMetadata {
        dirty: dirty.clone(),
        empty: empty - dirty,
        block_size: page_size,
    })
}

fn address_of(regions: &[GuestRegion], page: u32, page_size: u64) -> io::Result<u64> {
    let offset = u64::from(page) * page_size;
    regions
        .iter()
        .find(|region| (region.offset..region.offset + region.size).contains(&offset))
        .map(|region| region.base_host_virt_addr + offset - region.offset)
        .ok_or_else(|| io::Error::other(format!("memfile page {page} is outside guest memory")))
}

fn runs(regions: &[GuestRegion], pages: &RoaringBitmap, page_size: u64) -> io::Result<Vec<Run>> {
    let addresses = pages
        .iter()
        .map(|page| address_of(regions, page, page_size))
        .collect::<io::Result<Vec<u64>>>()?;
    Ok(addresses.into_iter().fold(Vec::new(), |mut runs: Vec<Run>, address| {
        match runs.last_mut() {
            Some(run) if run.address + run.length == address && run.length + page_size <= MAX_RUN_BYTES => {
                run.length += page_size
            }
            _ => runs.push(Run {
                address,
                length: page_size,
            }),
        }
        runs
    }))
}

fn offset_runs(pages: &RoaringBitmap, page_size: u64) -> Vec<Run> {
    pages
        .iter()
        .map(|page| u64::from(page) * page_size)
        .fold(Vec::new(), |mut runs: Vec<Run>, offset| {
            match runs.last_mut() {
                Some(run) if run.address + run.length == offset && run.length + page_size <= MAX_RUN_BYTES => {
                    run.length += page_size
                }
                _ => runs.push(Run {
                    address: offset,
                    length: page_size,
                }),
            }
            runs
        })
}

fn copy_memfd_run(memfd: &File, run: &Run, out: &mut impl Write) -> io::Result<()> {
    let mut bytes = vec![0u8; run.length as usize];
    memfd.read_exact_at(&mut bytes, run.address)?;
    out.write_all(&bytes)
}

fn copy_run(pid: u32, run: &Run, out: &mut impl Write) -> io::Result<()> {
    let mut bytes = vec![0u8; run.length as usize];
    let remote = [RemoteIoVec {
        base: run.address as usize,
        len: bytes.len(),
    }];
    let read = process_vm_readv(Pid::from_raw(pid as i32), &mut [IoSliceMut::new(&mut bytes)], &remote)?;
    if read != bytes.len() {
        return Err(io::Error::other(format!(
            "short read of guest memory at {:#x}: {read} of {} bytes",
            run.address,
            bytes.len()
        )));
    }
    out.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::uffd::testing::Pages;

    const PAGE: u64 = 4096;

    fn set(pages: &[u32]) -> RoaringBitmap {
        pages.iter().copied().collect()
    }

    #[test]
    fn exports_dirty_pages_compactly_and_records_empty_ones() {
        let pages = Pages::map(4 * PAGE as usize);
        let fill = |page: u64, value: u8| {
            (0..PAGE).for_each(|byte| Pages::write_byte(pages.address() + page * PAGE + byte, value))
        };
        fill(0, 0x11);
        fill(2, 0x22);
        let region = GuestRegion {
            base_host_virt_addr: pages.address(),
            size: 4 * PAGE,
            offset: 0,
            page_size: PAGE,
        };

        let mut out = Vec::new();
        let metadata = export_pages(
            &GuestMemory::Process {
                pid: std::process::id(),
            },
            &[region],
            &set(&[0, 2]),
            &set(&[1, 2]),
            &mut out,
        )
        .unwrap();

        let expected = [vec![0x11; PAGE as usize], vec![0x22; PAGE as usize]].concat();
        assert!(out == expected, "exported bytes differ");
        assert_eq!(metadata.dirty, set(&[0, 2]));
        assert_eq!(metadata.empty, set(&[1]), "a written page is dirty, not empty");
        assert_eq!(metadata.block_size, PAGE);
    }

    #[test]
    fn bitmap_indices_follow_the_region_order_into_memfile_pages() {
        let regions = [
            GuestRegion {
                base_host_virt_addr: 0x100000,
                size: 2 * PAGE,
                offset: 4 * PAGE,
                page_size: PAGE,
            },
            GuestRegion {
                base_host_virt_addr: 0x900000,
                size: 2 * PAGE,
                offset: 0,
                page_size: PAGE,
            },
        ];

        assert_eq!(memfile_pages(&regions, &set(&[0, 3])), set(&[4, 1]));
    }

    #[test]
    fn runs_coalesce_neighbouring_pages_up_to_a_byte_cap() {
        let region = GuestRegion {
            base_host_virt_addr: 0x10000,
            size: 2048 * PAGE,
            offset: 0,
            page_size: PAGE,
        };
        let pages: RoaringBitmap = (0..1500).chain([1600]).collect();

        let runs = runs(&[region], &pages, PAGE).unwrap();

        let shape: Vec<(u64, u64)> = runs.iter().map(|run| (run.address, run.length)).collect();
        assert_eq!(
            shape,
            vec![
                (0x10000, MAX_RUN_BYTES),
                (0x10000 + MAX_RUN_BYTES, 1500 * PAGE - MAX_RUN_BYTES),
                (0x10000 + 1600 * PAGE, PAGE)
            ]
        );
    }

    #[test]
    fn exports_dirty_pages_from_a_memfd_by_memfile_offset() {
        let mut memfd = tempfile::tempfile().unwrap();
        let content: Vec<u8> = (0..4u8).flat_map(|page| vec![page + 1; PAGE as usize]).collect();
        std::io::Write::write_all(&mut memfd, &content).unwrap();
        let region = GuestRegion {
            base_host_virt_addr: 0x7000_0000,
            size: 4 * PAGE,
            offset: 0,
            page_size: PAGE,
        };
        let mut out = Vec::new();

        export_pages(
            &GuestMemory::Memfd(Arc::new(memfd)),
            &[region],
            &set(&[1, 2]),
            &set(&[]),
            &mut out,
        )
        .unwrap();

        assert_eq!(out, [vec![2; PAGE as usize], vec![3; PAGE as usize]].concat());
    }
}
