use roaring::RoaringBitmap;

use crate::memory::region::Regions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteProtectRun {
    pub start: u64,
    pub length: u64,
}

pub fn coalesce_write_protect_runs(pages: &RoaringBitmap, regions: &Regions) -> Result<Vec<WriteProtectRun>, String> {
    let page_size = regions.page_size();
    let mut runs: Vec<WriteProtectRun> = Vec::new();
    for page in pages {
        let offset = u64::from(page) * page_size;
        let address = regions
            .address_of(offset)
            .ok_or_else(|| format!("translate page {page} (offset {offset:#x}): offset is outside guest memory"))?;
        match runs.last_mut() {
            Some(run) if run.start + run.length == address => run.length += page_size,
            _ => runs.push(WriteProtectRun {
                start: address,
                length: page_size,
            }),
        }
    }
    Ok(runs)
}
