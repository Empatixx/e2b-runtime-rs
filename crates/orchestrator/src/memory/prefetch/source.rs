use super::flags::PrefetchFlags;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefetchSource {
    Off,
    #[default]
    Init,
    LastCycle,
    Both,
}

impl PrefetchSource {
    pub fn parse(source: &str) -> Self {
        match source {
            "off" => Self::Off,
            "last-cycle" => Self::LastCycle,
            "both" => Self::Both,
            _ => Self::Init,
        }
    }

    pub fn uses_init(self) -> bool {
        matches!(self, Self::Init | Self::Both)
    }

    pub fn uses_last_cycle(self) -> bool {
        matches!(self, Self::LastCycle | Self::Both)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefetchSettings {
    pub source: PrefetchSource,
    pub last_cycle_max_mib: i64,
}

impl Default for PrefetchSettings {
    fn default() -> Self {
        Self {
            source: PrefetchSource::Init,
            last_cycle_max_mib: -1,
        }
    }
}

impl PrefetchSettings {
    pub fn from_flags(flags: &dyn PrefetchFlags) -> Self {
        let source = PrefetchSource::parse(&flags.resume_prefetch_source());
        Self {
            source,
            last_cycle_max_mib: if source.uses_last_cycle() {
                flags.resume_last_cycle_prefetch_max_mib()
            } else {
                Self::default().last_cycle_max_mib
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_selects_the_traces_like_upstream() {
        let selected = |source| {
            let source = PrefetchSource::parse(source);
            (source.uses_init(), source.uses_last_cycle())
        };

        assert_eq!(selected("off"), (false, false));
        assert_eq!(selected("init"), (true, false));
        assert_eq!(selected("last-cycle"), (false, true));
        assert_eq!(selected("both"), (true, true));
    }

    #[test]
    fn unknown_sources_fall_back_to_init() {
        assert_eq!(PrefetchSource::parse("everything"), PrefetchSource::Init);
        assert_eq!(PrefetchSource::parse(""), PrefetchSource::Init);
        assert_eq!(PrefetchSource::parse("Off"), PrefetchSource::Init);
    }

    #[test]
    fn the_defaults_are_the_flag_defaults() {
        assert_eq!(
            PrefetchSettings::default(),
            PrefetchSettings::from_flags(&super::super::flags::fixed::FixedPrefetchFlags::default())
        );
    }
}
