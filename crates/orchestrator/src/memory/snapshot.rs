use std::io::{self, IoSliceMut, Write};

use nix::sys::uio::{RemoteIoVec, process_vm_readv};
use nix::unistd::Pid;
use roaring::RoaringBitmap;

use crate::firecracker::memory_info::{GuestRegion, MemoryInfo};
use crate::storage::header::DiffMetadata;

const MAX_RUN_PAGES: u64 = 1024;

struct GuestPage {
    address: u64,
    memfile_page: u32,
}

struct Run {
    address: u64,
    pages: u64,
}

pub fn export_memfile(
    pid: u32,
    regions: &[GuestRegion],
    info: &MemoryInfo,
    out: &mut impl Write,
) -> io::Result<DiffMetadata> {
    let page_size = regions
        .first()
        .map(|region| region.page_size)
        .ok_or_else(|| io::Error::other("firecracker reported no memory regions"))?;
    let pages = guest_pages(regions);
    let dirty = &info.resident - &info.empty;
    runs(&pages, &dirty, page_size)
        .iter()
        .try_for_each(|run| copy_run(pid, run, page_size, out))?;
    Ok(DiffMetadata {
        dirty: memfile_pages(&pages, &dirty),
        empty: memfile_pages(&pages, &info.empty),
        block_size: page_size,
    })
}

fn guest_pages(regions: &[GuestRegion]) -> Vec<GuestPage> {
    regions
        .iter()
        .flat_map(|region| {
            (0..region.size / region.page_size).map(move |index| GuestPage {
                address: region.base_host_virt_addr + index * region.page_size,
                memfile_page: (region.offset / region.page_size + index) as u32,
            })
        })
        .collect()
}

fn runs(pages: &[GuestPage], indices: &RoaringBitmap, page_size: u64) -> Vec<Run> {
    indices
        .iter()
        .filter_map(|index| pages.get(index as usize))
        .fold(Vec::new(), |mut runs: Vec<Run>, page| {
            match runs.last_mut() {
                Some(run) if run.address + run.pages * page_size == page.address && run.pages < MAX_RUN_PAGES => {
                    run.pages += 1
                }
                _ => runs.push(Run {
                    address: page.address,
                    pages: 1,
                }),
            }
            runs
        })
}

fn copy_run(pid: u32, run: &Run, page_size: u64, out: &mut impl Write) -> io::Result<()> {
    let mut bytes = vec![0u8; (run.pages * page_size) as usize];
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

fn memfile_pages(pages: &[GuestPage], indices: &RoaringBitmap) -> RoaringBitmap {
    indices
        .iter()
        .filter_map(|index| pages.get(index as usize))
        .map(|page| page.memfile_page)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::uffd::testing::Pages;

    const PAGE: u64 = 4096;

    #[test]
    fn exports_resident_non_empty_pages_compactly() {
        let pages = Pages::map(4 * PAGE as usize);
        let fill = |page: u64, value: u8| {
            (0..PAGE).for_each(|byte| Pages::write_byte(pages.address() + page * PAGE + byte, value))
        };
        fill(0, 0x11);
        fill(1, 0);
        fill(2, 0x22);
        let region = GuestRegion {
            base_host_virt_addr: pages.address(),
            size: 4 * PAGE,
            offset: 0,
            page_size: PAGE,
        };
        let info = MemoryInfo {
            resident: [0, 1, 2].into_iter().collect(),
            empty: [1].into_iter().collect(),
        };

        let mut out = Vec::new();
        let metadata = export_memfile(std::process::id(), &[region], &info, &mut out).unwrap();

        let expected = [vec![0x11; PAGE as usize], vec![0x22; PAGE as usize]].concat();
        assert!(out == expected, "exported bytes differ");
        assert_eq!(metadata.dirty.iter().collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(metadata.empty.iter().collect::<Vec<_>>(), vec![1]);
        assert_eq!(metadata.block_size, PAGE);
    }

    #[test]
    fn runs_coalesce_neighbouring_pages() {
        let region = GuestRegion {
            base_host_virt_addr: 0x10000,
            size: 8 * PAGE,
            offset: 0,
            page_size: PAGE,
        };
        let pages = guest_pages(&[region]);
        let indices: RoaringBitmap = [1, 2, 3, 6].into_iter().collect();

        let runs = runs(&pages, &indices, PAGE);

        let shape: Vec<(u64, u64)> = runs.iter().map(|run| (run.address, run.pages)).collect();
        assert_eq!(shape, vec![(0x10000 + PAGE, 3), (0x10000 + 6 * PAGE, 1)]);
    }
}
