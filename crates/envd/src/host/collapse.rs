#[cfg(any(target_os = "linux", test))]
const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollapseStats {
    pub regions: usize,
    pub chunks: usize,
    pub collapsed: usize,
    pub already_huge: usize,
    pub skipped: usize,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    start: usize,
    end: usize,
}

#[cfg(target_os = "linux")]
pub fn collapse_self() -> Result<CollapseStats, String> {
    let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| format!("read self maps: {e}"))?;
    let before = anon_huge_pages_bytes();
    let mut stats = CollapseStats::default();
    let mut successes = 0;
    for region in anon_rw_regions(&maps) {
        stats.regions += 1;
        let range = collapse_range(region);
        stats.chunks += range.chunks;
        successes += range.collapsed;
        stats.skipped += range.skipped;
    }
    let after = anon_huge_pages_bytes();
    let (collapsed, already_huge) = split_collapsed(successes, before.zip(after));
    stats.collapsed = collapsed;
    stats.already_huge = already_huge;
    Ok(stats)
}

#[cfg(not(target_os = "linux"))]
pub fn collapse_self() -> Result<CollapseStats, String> {
    Ok(CollapseStats::default())
}

#[cfg(target_os = "linux")]
fn collapse_range(region: Region) -> CollapseStats {
    let mut stats = CollapseStats::default();
    let _ = madvise(region.start, region.end - region.start, nix::libc::MADV_HUGEPAGE);
    let first = (region.start + HUGE_PAGE_SIZE - 1) & !(HUGE_PAGE_SIZE - 1);
    let mut chunk = first;
    while chunk + HUGE_PAGE_SIZE <= region.end {
        stats.chunks += 1;
        match madvise(chunk, HUGE_PAGE_SIZE, MADV_COLLAPSE) {
            Ok(()) => stats.collapsed += 1,
            Err(_) => stats.skipped += 1,
        }
        chunk += HUGE_PAGE_SIZE;
    }
    stats
}

#[cfg(target_os = "linux")]
const MADV_COLLAPSE: i32 = 25;

#[cfg(target_os = "linux")]
fn madvise(address: usize, length: usize, advice: i32) -> Result<(), nix::errno::Errno> {
    let result = unsafe { nix::libc::syscall(nix::libc::SYS_madvise, address, length, advice) };
    match result {
        0 => Ok(()),
        _ => Err(nix::errno::Errno::last()),
    }
}

#[cfg(any(target_os = "linux", test))]
fn split_collapsed(successes: usize, measured: Option<(u64, u64)>) -> (usize, usize) {
    match measured {
        Some((before, after)) if after >= before => {
            let migrated = (((after - before) / HUGE_PAGE_SIZE as u64) as usize).min(successes);
            (migrated, successes - migrated)
        }
        _ => (successes, 0),
    }
}

#[cfg(target_os = "linux")]
fn anon_huge_pages_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .as_deref()
        .and_then(huge_pages_from_rollup)
}

#[cfg(any(target_os = "linux", test))]
fn huge_pages_from_rollup(rollup: &str) -> Option<u64> {
    rollup
        .lines()
        .find(|line| line.starts_with("AnonHugePages:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|kib| kib.parse::<u64>().ok())
        .map(|kib| kib * 1024)
}

#[cfg(any(target_os = "linux", test))]
fn anon_rw_regions(maps: &str) -> Vec<Region> {
    maps.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (range, perms) = (fields.first()?, fields.get(1)?);
            let anonymous = fields.len() <= 5;
            let readable_writable = perms.starts_with("rw");
            let (start, end) = range.split_once('-')?;
            (anonymous && readable_writable).then_some(Region {
                start: usize::from_str_radix(start, 16).ok()?,
                end: usize::from_str_radix(end, 16).ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_anonymous_read_write_mappings_are_collapsed() {
        let maps = "\
55d0c0000000-55d0c0200000 r--p 00000000 08:01 123 /usr/bin/envd
55d0c1000000-55d0c1400000 rw-p 00000000 00:00 0 [heap]
7f0000000000-7f0000600000 rw-p 00000000 00:00 0
7f0000800000-7f0000a00000 r--p 00000000 00:00 0
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0 [stack]
";
        assert_eq!(
            anon_rw_regions(maps),
            vec![Region {
                start: 0x7f00_0000_0000,
                end: 0x7f00_0060_0000
            }]
        );
    }

    #[test]
    fn collapsed_chunks_split_by_the_anon_huge_pages_delta() {
        let mib = 1024 * 1024;
        assert_eq!(split_collapsed(5, Some((0, 4 * mib))), (2, 3));
        assert_eq!(split_collapsed(1, Some((0, 8 * mib))), (1, 0));
        assert_eq!(split_collapsed(3, Some((8 * mib, 0))), (3, 0));
        assert_eq!(split_collapsed(3, None), (3, 0));
        assert_eq!(
            huge_pages_from_rollup("Rss: 1 kB\nAnonHugePages:  2048 kB\n"),
            Some(2 * mib)
        );
    }
}
