use super::client::FeatureFlags;
use super::contexts::FlagContext;
use super::flags::{
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
    }

    #[test]
    fn overrides_are_read() {
        let flags = FeatureFlags::builder()
            .override_flag("resume-prefetch-source", "both")
            .override_flag("pause-resume-prefetch-harvest", true)
            .offline()
            .scoped(Vec::new());

        assert_eq!(flags.resume_prefetch_source(), "both");
        assert!(flags.pause_resume_prefetch_harvest());
    }
}
