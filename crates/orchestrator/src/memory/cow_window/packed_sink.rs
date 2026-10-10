use std::io;
use std::sync::Arc;

use roaring::RoaringBitmap;

use super::io::PageSink;

pub struct PackedPageSink {
    pages: RoaringBitmap,
    page_size: u64,
    destination: Arc<dyn PageSink>,
}

impl PackedPageSink {
    pub fn new(pages: RoaringBitmap, page_size: u64, destination: Arc<dyn PageSink>) -> Self {
        Self {
            pages,
            page_size,
            destination,
        }
    }
}

impl PageSink for PackedPageSink {
    fn write_page(&self, data: &[u8], offset: u64) -> io::Result<()> {
        if !offset.is_multiple_of(self.page_size) || data.len() as u64 != self.page_size {
            return Err(io::Error::other(format!(
                "packed sink: write [{offset},{}) is not one aligned page",
                offset + data.len() as u64
            )));
        }
        let page = u32::try_from(offset / self.page_size)
            .ok()
            .filter(|page| self.pages.contains(*page))
            .ok_or_else(|| {
                io::Error::other(format!(
                    "packed sink: page {} is not in the captured set",
                    offset / self.page_size
                ))
            })?;
        let slot = self.pages.rank(page) - 1;
        self.destination.write_page(data, slot * self.page_size)
    }
}
