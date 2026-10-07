use std::ops::Range;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Region {
    pub base_host_virt_addr: u64,
    pub size: u64,
    pub offset: u64,
    #[serde(rename = "page_size_kib", alias = "page_size")]
    pub page_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Regions {
    regions: Vec<Region>,
    page_size: u64,
}

const SUPPORTED_PAGE_SIZES: [u64; 2] = [4096, 2 << 20];

impl Region {
    fn contains(&self, address: u64) -> bool {
        (self.base_host_virt_addr..self.base_host_virt_addr + self.size).contains(&address)
    }

    fn address_range(&self, offsets: &Range<u64>) -> Option<Range<u64>> {
        let start = offsets.start.max(self.offset);
        let end = offsets.end.min(self.offset + self.size);
        (start < end).then(|| self.address_at(start)..self.address_at(end))
    }

    fn address_at(&self, offset: u64) -> u64 {
        offset - self.offset + self.base_host_virt_addr
    }
}

impl Regions {
    pub fn parse(json: &[u8]) -> Result<Self, String> {
        let regions: Vec<Region> = serde_json::from_slice(json).map_err(|e| format!("parse uffd regions: {e}"))?;
        let page_size = shared_page_size(&regions)?;
        Ok(Self { regions, page_size })
    }

    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    pub fn total_size(&self) -> u64 {
        self.regions.iter().map(|region| region.size).sum()
    }

    pub fn offset_of(&self, address: u64) -> Result<u64, String> {
        let page = address & !(self.page_size - 1);
        self.regions
            .iter()
            .find(|region| region.contains(page))
            .map(|region| page - region.base_host_virt_addr + region.offset)
            .ok_or_else(|| format!("address {address:#x} is outside guest memory"))
    }

    pub fn addresses_of(&self, offsets: Range<u64>) -> Vec<Range<u64>> {
        self.regions
            .iter()
            .filter_map(|region| region.address_range(&offsets))
            .collect()
    }
}

fn shared_page_size(regions: &[Region]) -> Result<u64, String> {
    let page_size = regions
        .first()
        .map(|region| region.page_size)
        .ok_or("uffd handshake sent no memory regions")?;
    if regions.iter().any(|region| region.page_size != page_size) {
        return Err("uffd regions use different page sizes".to_owned());
    }
    SUPPORTED_PAGE_SIZES
        .contains(&page_size)
        .then_some(page_size)
        .ok_or_else(|| format!("unsupported uffd page size {page_size}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = 4096;

    fn regions() -> Regions {
        Regions::parse(
            br#"[{"base_host_virt_addr":1048576,"size":16384,"offset":0,"page_size_kib":4096},
                 {"base_host_virt_addr":8388608,"size":8192,"offset":16384,"page_size_kib":4096}]"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_both_page_size_spellings() {
        let legacy = Regions::parse(br#"[{"base_host_virt_addr":0,"size":4096,"offset":0,"page_size_kib":4096}]"#);
        let current = Regions::parse(br#"[{"base_host_virt_addr":0,"size":4096,"offset":0,"page_size":2097152}]"#);
        assert_eq!(legacy.unwrap().page_size(), PAGE);
        assert_eq!(current.unwrap().page_size(), 2 << 20);
    }

    #[test]
    fn rejects_mixed_unsupported_and_empty_page_sizes() {
        let mixed = Regions::parse(
            br#"[{"base_host_virt_addr":0,"size":4096,"offset":0,"page_size_kib":4096},
                 {"base_host_virt_addr":4096,"size":2097152,"offset":4096,"page_size_kib":2097152}]"#,
        );
        assert_eq!(mixed.unwrap_err(), "uffd regions use different page sizes");
        let odd = Regions::parse(br#"[{"base_host_virt_addr":0,"size":8192,"offset":0,"page_size_kib":8192}]"#);
        assert_eq!(odd.unwrap_err(), "unsupported uffd page size 8192");
        assert_eq!(
            Regions::parse(b"[]").unwrap_err(),
            "uffd handshake sent no memory regions"
        );
        assert!(Regions::parse(b"{").unwrap_err().starts_with("parse uffd regions: "));
    }

    #[test]
    fn offsets_follow_the_region_and_round_down_to_the_page() {
        let regions = regions();
        assert_eq!(regions.offset_of(1048576), Ok(0));
        assert_eq!(regions.offset_of(1048576 + PAGE + 123), Ok(PAGE));
        assert_eq!(regions.offset_of(8388608 + PAGE), Ok(16384 + PAGE));
        assert_eq!(
            regions.offset_of(1048576 + 16384),
            Err("address 0x104000 is outside guest memory".to_owned())
        );
    }

    #[test]
    fn memfile_ranges_map_back_to_addresses() {
        let regions = regions();
        assert_eq!(
            regions.addresses_of(PAGE..16384 + PAGE),
            vec![1048576 + PAGE..1048576 + 16384, 8388608..8388608 + PAGE]
        );
        assert_eq!(regions.total_size(), 24576);
    }
}
