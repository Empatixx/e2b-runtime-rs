use std::collections::HashMap;
use std::num::NonZeroUsize;

use tracing_subscriber::filter::LevelFilter;

use crate::go_strconv::parse_bool;

pub const LOG_LEVEL_VARIABLE: &str = "NFS_PROXY_LOG_LEVEL";
const DEFAULT_HANDLE_CACHE_LIMIT: i64 = 16384;
const MIN_HANDLE_CACHE_LIMIT: i64 = 2;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum NfsLogLevel {
    Panic,
    Fatal,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl NfsLogLevel {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.to_lowercase().as_str() {
            "panic" => Ok(Self::Panic),
            "fatal" => Ok(Self::Fatal),
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            other => Err(format!("invalid log level {other:?}")),
        }
    }

    pub fn from_env() -> Self {
        std::env::var(LOG_LEVEL_VARIABLE)
            .ok()
            .and_then(|text| Self::parse(&text).ok())
            .unwrap_or_default()
    }

    pub fn filter(self) -> LevelFilter {
        match self {
            Self::Panic | Self::Fatal | Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Debug => LevelFilter::DEBUG,
            Self::Trace => LevelFilter::TRACE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NfsProxySettings {
    pub logging: bool,
    pub tracing: bool,
    pub metrics: bool,
    pub record_handle_calls: bool,
    pub record_stat_calls: bool,
    pub handle_cache_limit: i64,
    pub log_level: NfsLogLevel,
}

impl Default for NfsProxySettings {
    fn default() -> Self {
        Self {
            logging: false,
            tracing: false,
            metrics: true,
            record_handle_calls: false,
            record_stat_calls: false,
            handle_cache_limit: DEFAULT_HANDLE_CACHE_LIMIT,
            log_level: NfsLogLevel::Info,
        }
    }
}

impl NfsProxySettings {
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        let defaults = Self::default();
        Ok(Self {
            logging: flag(vars, "NFS_PROXY_LOGGING", defaults.logging)?,
            tracing: flag(vars, "NFS_PROXY_TRACING", defaults.tracing)?,
            metrics: flag(vars, "NFS_PROXY_METRICS", defaults.metrics)?,
            record_handle_calls: flag(vars, "NFS_PROXY_RECORD_HANDLE_CALLS", defaults.record_handle_calls)?,
            record_stat_calls: flag(vars, "NFS_PROXY_RECORD_STAT_CALLS", defaults.record_stat_calls)?,
            handle_cache_limit: integer(vars, "NFS_PROXY_HANDLE_CACHE_LIMIT", defaults.handle_cache_limit)?,
            log_level: set(vars, LOG_LEVEL_VARIABLE).map_or(Ok(defaults.log_level), |text| {
                NfsLogLevel::parse(text).map_err(|e| format!("{LOG_LEVEL_VARIABLE}: {e}"))
            })?,
        })
    }

    pub fn handle_limit(&self) -> Result<NonZeroUsize, String> {
        match self.handle_cache_limit {
            0 => Ok(DEFAULT_HANDLE_CACHE_LIMIT),
            limit if limit < MIN_HANDLE_CACHE_LIMIT => {
                Err(format!("nfs proxy handle cache limit must be at least 2, got {limit}"))
            }
            limit => Ok(limit),
        }
        .map(|limit| {
            usize::try_from(limit)
                .ok()
                .and_then(NonZeroUsize::new)
                .unwrap_or(NonZeroUsize::MIN)
        })
    }
}

fn set<'a>(vars: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    vars.get(name).map(String::as_str).filter(|value| !value.is_empty())
}

fn flag(vars: &HashMap<String, String>, name: &str, default: bool) -> Result<bool, String> {
    set(vars, name).map_or(Ok(default), |value| {
        parse_bool(value).map_err(|e| format!("{name}: {e}"))
    })
}

fn integer(vars: &HashMap<String, String>, name: &str, default: i64) -> Result<i64, String> {
    set(vars, name).map_or(Ok(default), |value| {
        value
            .parse::<i64>()
            .map_err(|e| format!("{name}: invalid integer {value:?}: {e}"))
    })
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
    fn defaults_match_go() {
        let settings = NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_METRICS", "")])).unwrap();
        assert_eq!(settings, NfsProxySettings::default());
        assert!(settings.metrics);
        assert_eq!(settings.handle_limit().unwrap().get(), 16384);
    }

    #[test]
    fn settings_are_read_from_the_environment() {
        let settings = NfsProxySettings::from_vars(&vars(&[
            ("NFS_PROXY_LOGGING", "true"),
            ("NFS_PROXY_TRACING", "1"),
            ("NFS_PROXY_METRICS", "false"),
            ("NFS_PROXY_RECORD_HANDLE_CALLS", "T"),
            ("NFS_PROXY_RECORD_STAT_CALLS", "true"),
            ("NFS_PROXY_HANDLE_CACHE_LIMIT", "64"),
        ]))
        .unwrap();
        assert_eq!(
            settings,
            NfsProxySettings {
                logging: true,
                tracing: true,
                metrics: false,
                record_handle_calls: true,
                record_stat_calls: true,
                handle_cache_limit: 64,
                log_level: NfsLogLevel::Info,
            }
        );
    }

    #[test]
    fn the_log_level_parses_like_go() {
        let level =
            |text: &str| NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_LOG_LEVEL", text)])).map(|s| s.log_level);
        assert_eq!(level("DEBUG"), Ok(NfsLogLevel::Debug));
        assert_eq!(level(""), Ok(NfsLogLevel::Info));
        assert_eq!(
            level("Loud"),
            Err("NFS_PROXY_LOG_LEVEL: invalid log level \"loud\"".to_owned())
        );
        assert_eq!(NfsLogLevel::Fatal.filter(), LevelFilter::ERROR);
    }

    #[test]
    fn a_zero_limit_means_the_default() {
        let settings = NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_HANDLE_CACHE_LIMIT", "0")])).unwrap();
        assert_eq!(settings.handle_limit().unwrap().get(), 16384);
    }

    #[test]
    fn a_limit_below_two_is_rejected_like_go() {
        let settings = NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_HANDLE_CACHE_LIMIT", "-3")])).unwrap();
        assert_eq!(
            settings.handle_limit().unwrap_err(),
            "nfs proxy handle cache limit must be at least 2, got -3"
        );
    }

    #[test]
    fn bad_values_name_the_variable() {
        assert_eq!(
            NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_HANDLE_CACHE_LIMIT", "lots")])).unwrap_err(),
            "NFS_PROXY_HANDLE_CACHE_LIMIT: invalid integer \"lots\": invalid digit found in string"
        );
        assert_eq!(
            NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_METRICS", "yes")])).unwrap_err(),
            "NFS_PROXY_METRICS: invalid boolean \"yes\""
        );
    }
}
