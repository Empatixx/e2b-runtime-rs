use std::collections::HashMap;

use crate::go_strconv::parse_bool;

const DEFAULT_SOURCE: &str = "init";
const DEFAULT_LAST_CYCLE_MAX_MIB: i64 = -1;
const DEFAULT_HARVEST_TIMEOUT_MS: i64 = 15_000;

pub trait PrefetchFlags: Send + Sync {
    fn resume_prefetch_source(&self) -> String;
    fn resume_last_cycle_prefetch_max_mib(&self) -> i64;
    fn pause_resume_prefetch_harvest(&self) -> bool;
    fn pause_resume_prefetch_consume(&self) -> bool;
    fn pause_resume_prefetch_harvest_timeout_ms(&self) -> i64;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPrefetchFlags {
    pub source: String,
    pub last_cycle_max_mib: i64,
    pub harvest: bool,
    pub consume: bool,
    pub harvest_timeout_ms: i64,
}

impl Default for EnvPrefetchFlags {
    fn default() -> Self {
        Self::from_vars(&HashMap::new())
    }
}

impl EnvPrefetchFlags {
    pub fn from_vars(vars: &HashMap<String, String>) -> Self {
        Self {
            source: vars
                .get("RESUME_PREFETCH_SOURCE")
                .cloned()
                .unwrap_or_else(|| DEFAULT_SOURCE.to_owned()),
            last_cycle_max_mib: number(vars, "RESUME_LAST_CYCLE_PREFETCH_MAX_MIB", DEFAULT_LAST_CYCLE_MAX_MIB),
            harvest: switch(vars, "PAUSE_RESUME_PREFETCH_HARVEST"),
            consume: switch(vars, "PAUSE_RESUME_PREFETCH_CONSUME"),
            harvest_timeout_ms: number(
                vars,
                "PAUSE_RESUME_PREFETCH_HARVEST_TIMEOUT_MS",
                DEFAULT_HARVEST_TIMEOUT_MS,
            ),
        }
    }
}

impl PrefetchFlags for EnvPrefetchFlags {
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

fn number(vars: &HashMap<String, String>, name: &str, fallback: i64) -> i64 {
    vars.get(name).and_then(|value| value.parse().ok()).unwrap_or(fallback)
}

fn switch(vars: &HashMap<String, String>, name: &str) -> bool {
    vars.get(name).and_then(|value| parse_bool(value).ok()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_defaults_are_the_upstream_flag_defaults() {
        let flags = EnvPrefetchFlags::default();

        assert_eq!(flags.resume_prefetch_source(), "init");
        assert_eq!(flags.resume_last_cycle_prefetch_max_mib(), -1);
        assert!(!flags.pause_resume_prefetch_harvest());
        assert!(!flags.pause_resume_prefetch_consume());
        assert_eq!(flags.pause_resume_prefetch_harvest_timeout_ms(), 15_000);
    }

    #[test]
    fn the_environment_overrides_the_defaults() {
        let flags = EnvPrefetchFlags::from_vars(&vars(&[
            ("RESUME_PREFETCH_SOURCE", "both"),
            ("RESUME_LAST_CYCLE_PREFETCH_MAX_MIB", "64"),
            ("PAUSE_RESUME_PREFETCH_HARVEST", "true"),
            ("PAUSE_RESUME_PREFETCH_CONSUME", "1"),
            ("PAUSE_RESUME_PREFETCH_HARVEST_TIMEOUT_MS", "2500"),
        ]));

        assert_eq!(flags.resume_prefetch_source(), "both");
        assert_eq!(flags.resume_last_cycle_prefetch_max_mib(), 64);
        assert!(flags.pause_resume_prefetch_harvest());
        assert!(flags.pause_resume_prefetch_consume());
        assert_eq!(flags.pause_resume_prefetch_harvest_timeout_ms(), 2500);
    }

    #[test]
    fn unreadable_values_fall_back_to_the_defaults() {
        let flags = EnvPrefetchFlags::from_vars(&vars(&[
            ("RESUME_LAST_CYCLE_PREFETCH_MAX_MIB", "lots"),
            ("PAUSE_RESUME_PREFETCH_HARVEST", "maybe"),
            ("PAUSE_RESUME_PREFETCH_HARVEST_TIMEOUT_MS", ""),
        ]));

        assert_eq!(flags, EnvPrefetchFlags::default());
    }
}
