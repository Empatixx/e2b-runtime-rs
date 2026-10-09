use super::flags::PrefetchFlags;

const BYTES_PER_MIB: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefetchWorkers {
    pub fetch: usize,
    pub copy: usize,
    pub coalesce_max_mib: u64,
}

impl Default for PrefetchWorkers {
    fn default() -> Self {
        Self {
            fetch: 16,
            copy: 8,
            coalesce_max_mib: 0,
        }
    }
}

impl PrefetchWorkers {
    pub fn from_flags(flags: &dyn PrefetchFlags) -> Self {
        Self {
            fetch: non_negative(flags.memory_prefetch_max_fetch_workers()),
            copy: non_negative(flags.memory_prefetch_max_copy_workers()),
            coalesce_max_mib: non_negative(flags.memory_prefetch_coalesce_max_mb()),
        }
    }

    pub fn extent_blocks(&self, block_size: u64) -> usize {
        self.coalesce_max_mib
            .checked_mul(BYTES_PER_MIB)
            .zip(Some(block_size).filter(|size| *size > 0))
            .map_or(1, |(bytes, size)| {
                usize::try_from(bytes / size).unwrap_or(usize::MAX).max(1)
            })
    }
}

fn non_negative<T: TryFrom<i64> + Default>(value: i64) -> T {
    T::try_from(value.max(0)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::flags::fixed::FixedPrefetchFlags;
    use super::*;

    #[test]
    fn the_defaults_are_the_flag_defaults() {
        assert_eq!(
            PrefetchWorkers::default(),
            PrefetchWorkers::from_flags(&FixedPrefetchFlags::default())
        );
    }

    #[test]
    fn negative_flag_values_turn_a_stage_off() {
        let flags = FixedPrefetchFlags {
            fetch_workers: -3,
            copy_workers: -1,
            coalesce_max_mb: -8,
            ..FixedPrefetchFlags::default()
        };

        assert_eq!(
            PrefetchWorkers::from_flags(&flags),
            PrefetchWorkers {
                fetch: 0,
                copy: 0,
                coalesce_max_mib: 0
            }
        );
    }

    #[test]
    fn coalescing_off_fetches_single_blocks() {
        assert_eq!(PrefetchWorkers::default().extent_blocks(4096), 1);
    }

    #[test]
    fn the_extent_cap_is_the_mib_budget_in_blocks() {
        let workers = PrefetchWorkers {
            coalesce_max_mib: 2,
            ..PrefetchWorkers::default()
        };

        assert_eq!(workers.extent_blocks(4096), 512);
        assert_eq!(workers.extent_blocks(4 << 20), 1);
        assert_eq!(workers.extent_blocks(0), 1);
    }
}
