use super::client::FeatureFlags;
use super::contexts::FlagContext;
use super::flags::{
    MEMORY_PREFETCH_COALESCE_MAX_MB, MEMORY_PREFETCH_MAX_COPY_WORKERS, MEMORY_PREFETCH_MAX_FETCH_WORKERS,
    PAUSE_RESUME_PREFETCH_CONSUME, PAUSE_RESUME_PREFETCH_HARVEST, PAUSE_RESUME_PREFETCH_HARVEST_TIMEOUT_MS,
    RESUME_LAST_CYCLE_PREFETCH_MAX_MIB, resume_prefetch_source,
};
use crate::memory::prefetch::flags::PrefetchFlags;

#[derive(Debug, Clone)]
pub struct ScopedPrefetchFlags {
    flags: FeatureFlags,
    contexts: Vec<FlagContext>,
}

impl FeatureFlags {
    pub fn scoped(&self, contexts: Vec<FlagContext>) -> ScopedPrefetchFlags {
        ScopedPrefetchFlags {
            flags: self.clone(),
            contexts,
        }
    }
}

impl PrefetchFlags for ScopedPrefetchFlags {
    fn resume_prefetch_source(&self) -> String {
        self.flags.get(&resume_prefetch_source(), &self.contexts)
    }

    fn resume_last_cycle_prefetch_max_mib(&self) -> i64 {
        self.flags.get(&RESUME_LAST_CYCLE_PREFETCH_MAX_MIB, &self.contexts)
    }

    fn pause_resume_prefetch_harvest(&self) -> bool {
        self.flags.get(&PAUSE_RESUME_PREFETCH_HARVEST, &self.contexts)
    }

    fn pause_resume_prefetch_consume(&self) -> bool {
        self.flags.get(&PAUSE_RESUME_PREFETCH_CONSUME, &self.contexts)
    }

    fn pause_resume_prefetch_harvest_timeout_ms(&self) -> i64 {
        self.flags
            .get(&PAUSE_RESUME_PREFETCH_HARVEST_TIMEOUT_MS, &self.contexts)
    }

    fn memory_prefetch_max_fetch_workers(&self) -> i64 {
        self.flags.get(&MEMORY_PREFETCH_MAX_FETCH_WORKERS, &self.contexts)
    }

    fn memory_prefetch_max_copy_workers(&self) -> i64 {
        self.flags.get(&MEMORY_PREFETCH_MAX_COPY_WORKERS, &self.contexts)
    }

    fn memory_prefetch_coalesce_max_mb(&self) -> i64 {
        self.flags.get(&MEMORY_PREFETCH_COALESCE_MAX_MB, &self.contexts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_flags_serve_the_upstream_defaults() {
        let flags = FeatureFlags::offline().scoped(vec![FlagContext::sandbox("s1")]);

        assert_eq!(flags.resume_prefetch_source(), "init");
        assert_eq!(flags.resume_last_cycle_prefetch_max_mib(), -1);
        assert!(!flags.pause_resume_prefetch_harvest());
        assert!(!flags.pause_resume_prefetch_consume());
        assert_eq!(flags.pause_resume_prefetch_harvest_timeout_ms(), 15_000);
        assert_eq!(flags.memory_prefetch_max_fetch_workers(), 16);
        assert_eq!(flags.memory_prefetch_max_copy_workers(), 8);
        assert_eq!(flags.memory_prefetch_coalesce_max_mb(), 0);
    }

    #[test]
    fn overrides_are_read() {
        let flags = FeatureFlags::builder()
            .override_flag("resume-prefetch-source", "both")
            .override_flag("pause-resume-prefetch-harvest", true)
            .override_flag("memory-prefetch-max-fetch-workers", 4)
            .offline()
            .scoped(Vec::new());

        assert_eq!(flags.resume_prefetch_source(), "both");
        assert!(flags.pause_resume_prefetch_harvest());
        assert_eq!(flags.memory_prefetch_max_fetch_workers(), 4);
    }
}
