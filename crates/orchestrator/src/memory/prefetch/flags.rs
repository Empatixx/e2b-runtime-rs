pub trait PrefetchFlags: Send + Sync {
    fn resume_prefetch_source(&self) -> String;
    fn resume_last_cycle_prefetch_max_mib(&self) -> i64;
    fn pause_resume_prefetch_harvest(&self) -> bool;
    fn pause_resume_prefetch_consume(&self) -> bool;
    fn pause_resume_prefetch_harvest_timeout_ms(&self) -> i64;
}

#[cfg(test)]
pub mod fixed {
    use super::PrefetchFlags;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FixedPrefetchFlags {
        pub source: String,
        pub last_cycle_max_mib: i64,
        pub harvest: bool,
        pub consume: bool,
        pub harvest_timeout_ms: i64,
    }

    impl Default for FixedPrefetchFlags {
        fn default() -> Self {
            Self {
                source: "init".to_owned(),
                last_cycle_max_mib: -1,
                harvest: false,
                consume: false,
                harvest_timeout_ms: 15_000,
            }
        }
    }

    impl PrefetchFlags for FixedPrefetchFlags {
        fn resume_prefetch_source(&self) -> String {
            self.source.clone()
        }

        fn resume_last_cycle_prefetch_max_mib(&self) -> i64 {
            self.last_cycle_max_mib
        }

        fn pause_resume_prefetch_harvest(&self) -> bool {
            self.harvest
        }

        fn pause_resume_prefetch_consume(&self) -> bool {
            self.consume
        }

        fn pause_resume_prefetch_harvest_timeout_ms(&self) -> i64 {
            self.harvest_timeout_ms
        }
    }
}
