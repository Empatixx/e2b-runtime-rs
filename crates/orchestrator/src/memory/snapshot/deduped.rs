use std::io::{self, Write};
use std::sync::Arc;

use roaring::RoaringBitmap;
use tokio::runtime::Handle;

use super::{GuestMemory, diff_metadata};
use crate::block::build_file::BuildFile;
use crate::firecracker::memory_info::GuestRegion;
use crate::memory::dedup::compare::{BasePages, Comparison, compare};
use crate::memory::dedup::config::DedupConfig;
use crate::memory::dedup::drain::drain_dirty_pages;
use crate::storage::header::{DiffMetadata, Header, PAGE_SIZE};

pub struct BuildFileBase {
    pub file: Arc<BuildFile>,
    pub runtime: Handle,
}

impl BasePages for BuildFileBase {
    fn header(&self) -> &Header {
        self.file.header()
    }

    fn page(&self, offset: u64) -> Result<Vec<u8>, String> {
        self.runtime
            .block_on(self.file.slice(offset, PAGE_SIZE as usize))
            .map(|page| page.to_vec())
    }

    fn is_cached(&self, offset: u64) -> bool {
        self.runtime.block_on(self.file.is_cached(offset, PAGE_SIZE as usize))
    }
}

pub struct DedupPlan {
    pub diff: DiffMetadata,
    block_size: u64,
}

pub fn export_deduped(
    memory: &GuestMemory,
    regions: &[GuestRegion],
    dirty: &RoaringBitmap,
    empty: &RoaringBitmap,
    base: &dyn BasePages,
    config: &DedupConfig,
    out: &mut impl Write,
) -> io::Result<DiffMetadata> {
    let plan = plan_deduped(memory, regions, dirty, empty, base, config)?;
    drain_deduped(memory, regions, &plan, out)?;
    Ok(plan.diff)
}

pub fn plan_deduped(
    memory: &GuestMemory,
    regions: &[GuestRegion],
    dirty: &RoaringBitmap,
    empty: &RoaringBitmap,
    base: &dyn BasePages,
    config: &DedupConfig,
) -> io::Result<DedupPlan> {
    let block_size = diff_metadata(regions, dirty, empty)?.block_size;
    let mut source = |absolute: u64| read_block(memory, regions, absolute / block_size, block_size);
    let mut plan = compare(
        &mut source,
        base,
        &Comparison {
            dirty,
            block_size,
            best_effort: config.best_effort,
            budget: config.budget,
        },
    )
    .map_err(io::Error::other)?;
    let ratio = block_size / PAGE_SIZE;
    empty.iter().for_each(|block| {
        let first = u64::from(block) * ratio;
        plan.page_empty.insert_range(first as u32..(first + ratio) as u32);
    });
    Ok(DedupPlan {
        diff: DiffMetadata {
            empty: &plan.page_empty - &plan.page_dirty,
            dirty: plan.page_dirty,
            block_size: PAGE_SIZE,
        },
        block_size,
    })
}

pub fn drain_deduped(
    memory: &GuestMemory,
    regions: &[GuestRegion],
    plan: &DedupPlan,
    out: &mut impl Write,
) -> io::Result<()> {
    let block_size = plan.block_size;
    let mut source = |absolute: u64| read_block(memory, regions, absolute / block_size, block_size);
    drain_dirty_pages(&mut source, &plan.diff.dirty, block_size, out).map(drop)
}

fn read_block(memory: &GuestMemory, regions: &[GuestRegion], block: u64, block_size: u64) -> Result<Vec<u8>, String> {
    let pages: RoaringBitmap = [block as u32].into_iter().collect();
    let mut bytes = Vec::with_capacity(block_size as usize);
    memory
        .copy(regions, &pages, block_size, &mut bytes)
        .map_err(|e| format!("read block {block}: {e}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::memory::uffd::testing::Pages;
    use crate::storage::header::{BuildMapping, Metadata};

    const PAGE: u64 = PAGE_SIZE;

    struct Base {
        data: Vec<u8>,
        header: Header,
    }

    impl BasePages for Base {
        fn header(&self) -> &Header {
            &self.header
        }

        fn page(&self, offset: u64) -> Result<Vec<u8>, String> {
            Ok(self.data[offset as usize..(offset + PAGE) as usize].to_vec())
        }

        fn is_cached(&self, _: u64) -> bool {
            true
        }
    }

    fn set(pages: &[u32]) -> RoaringBitmap {
        pages.iter().copied().collect()
    }

    #[test]
    fn only_pages_that_differ_from_the_base_are_stored_and_zero_pages_join_the_empty_set() {
        let build = Uuid::from_u128(1);
        let base_data: Vec<u8> = (0..4u8).flat_map(|page| vec![page + 1; PAGE as usize]).collect();
        let base = Base {
            header: Header::new(
                Metadata::template(build, PAGE, 4 * PAGE),
                vec![BuildMapping {
                    offset: 0,
                    length: 4 * PAGE,
                    build_id: build,
                    storage_offset: 0,
                }],
            )
            .unwrap(),
            data: base_data.clone(),
        };
        let pages = Pages::map(4 * PAGE as usize);
        (0..4 * PAGE).for_each(|byte| {
            let page = byte / PAGE;
            let value = match page {
                1 => 0xEE,
                2 => 0,
                _ => base_data[byte as usize],
            };
            pages.write_byte(byte as usize, value);
        });
        let region = GuestRegion {
            base_host_virt_addr: pages.address(),
            size: 4 * PAGE,
            offset: 0,
            page_size: PAGE,
        };
        let memory = GuestMemory::Process {
            pid: std::process::id(),
        };
        let mut out = Vec::new();

        let diff = export_deduped(
            &memory,
            &[region],
            &set(&[0, 1, 2, 3]),
            &set(&[]),
            &base,
            &DedupConfig::default(),
            &mut out,
        )
        .unwrap();

        assert_eq!(diff.dirty, set(&[1]));
        assert_eq!(diff.empty, set(&[2]));
        assert_eq!(diff.block_size, PAGE);
        assert!(out == vec![0xEE; PAGE as usize], "only page 1 is stored");
    }
}
