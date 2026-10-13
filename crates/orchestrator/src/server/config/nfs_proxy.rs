use std::collections::HashMap;
use std::num::{IntErrorKind, NonZeroUsize};

use crate::go_strconv::{parse_bool, quote};

const DEFAULT_HANDLE_CACHE_LIMIT: i64 = 16384;
const MIN_HANDLE_CACHE_LIMIT: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NfsProxySettings {
    pub logging: bool,
    pub tracing: bool,
    pub metrics: bool,
    pub record_handle_calls: bool,
    pub record_stat_calls: bool,
    pub handle_cache_limit: i64,
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
        }
    }
}

impl NfsProxySettings {
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        let defaults = Self::default();
        Ok(Self {
            logging: flag(vars, "NFS_PROXY_LOGGING", "NFSProxyLogging", defaults.logging)?,
            tracing: flag(vars, "NFS_PROXY_TRACING", "NFSProxyTracing", defaults.tracing)?,
            metrics: flag(vars, "NFS_PROXY_METRICS", "NFSProxyMetrics", defaults.metrics)?,
            record_handle_calls: flag(
                vars,
                "NFS_PROXY_RECORD_HANDLE_CALLS",
                "NFSProxyRecordHandleCalls",
                defaults.record_handle_calls,
            )?,
            record_stat_calls: flag(
                vars,
                "NFS_PROXY_RECORD_STAT_CALLS",
                "NFSProxyRecordStatCalls",
                defaults.record_stat_calls,
            )?,
            handle_cache_limit: integer(
                vars,
                "NFS_PROXY_HANDLE_CACHE_LIMIT",
                "NFSProxyHandleCacheLimit",
                defaults.handle_cache_limit,
            )?,
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

fn flag(vars: &HashMap<String, String>, name: &str, field: &str, default: bool) -> Result<bool, String> {
    set(vars, name).map_or(Ok(default), |value| {
        parse_bool(value).map_err(|e| format!("env: parse error on field {} of type \"bool\": {e}", quote(field)))
    })
}

fn integer(vars: &HashMap<String, String>, name: &str, field: &str, default: i64) -> Result<i64, String> {
    set(vars, name).map_or(Ok(default), |value| {
        value.parse::<i64>().map_err(|e| {
            let reason = match e.kind() {
                IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => "value out of range",
                _ => "invalid syntax",
            };
            format!(
                "env: parse error on field {} of type \"int\": strconv.ParseInt: parsing {}: {reason}",
                quote(field),
                quote(value)
            )
        })
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
            }
        );
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
    fn bad_values_fail_like_the_go_env_parser() {
        assert_eq!(
            NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_HANDLE_CACHE_LIMIT", "lots")])).unwrap_err(),
            "env: parse error on field \"NFSProxyHandleCacheLimit\" of type \"int\": strconv.ParseInt: parsing \"lots\": invalid syntax"
        );
        assert_eq!(
            NfsProxySettings::from_vars(&vars(&[("NFS_PROXY_METRICS", "yes")])).unwrap_err(),
            "env: parse error on field \"NFSProxyMetrics\" of type \"bool\": strconv.ParseBool: parsing \"yes\": invalid syntax"
        );
    }
}
