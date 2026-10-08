use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use crate::proto::{LogLevel, LogsDirection, TemplateBuildLogEntry};

pub const MAX_ENTRIES_PER_REQUEST: u32 = 100;
pub const DEFAULT_TIME_RANGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: SystemTime,
    pub message: String,
    pub level: LogLevel,
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub level: i32,
    pub start: Option<SystemTime>,
    pub end: Option<SystemTime>,
    pub direction: LogsDirection,
    pub offset: u32,
    pub limit: Option<u32>,
}

#[derive(Default)]
pub struct BuildLog {
    entries: Mutex<Vec<LogEntry>>,
}

#[derive(Clone, Copy)]
pub struct ScopedLog<'a> {
    pub log: &'a BuildLog,
    pub fields: &'a BTreeMap<String, String>,
}

impl ScopedLog<'_> {
    pub fn push(&self, level: LogLevel, message: impl Into<String>) {
        self.log.push_with(level, message, self.fields.clone());
    }
}

impl BuildLog {
    pub fn push(&self, level: LogLevel, message: impl Into<String>) {
        self.push_with(level, message, BTreeMap::new());
    }

    pub fn push_with(&self, level: LogLevel, message: impl Into<String>, fields: BTreeMap<String, String>) {
        self.entries.lock().push(LogEntry {
            timestamp: SystemTime::now(),
            message: message.into(),
            level,
            fields,
        });
    }

    pub fn scoped<'a>(&'a self, fields: &'a BTreeMap<String, String>) -> ScopedLog<'a> {
        ScopedLog { log: self, fields }
    }

    pub fn query(&self, query: &LogQuery) -> Vec<TemplateBuildLogEntry> {
        let now = SystemTime::now();
        let start = query.start.unwrap_or(now - DEFAULT_TIME_RANGE);
        let end = query.end.unwrap_or(now);
        let limit = query
            .limit
            .map_or(MAX_ENTRIES_PER_REQUEST, |limit| limit.min(MAX_ENTRIES_PER_REQUEST));
        let mut entries = self.entries.lock().clone();
        match query.direction {
            LogsDirection::Backward => entries.sort_by_key(|entry| std::cmp::Reverse(entry.timestamp)),
            LogsDirection::Forward => entries.sort_by_key(|entry| entry.timestamp),
        }
        entries
            .into_iter()
            .filter(|entry| entry.level as i32 >= query.level)
            .filter(|entry| entry.timestamp >= start && entry.timestamp <= end)
            .skip(query.offset as usize)
            .take(limit as usize)
            .map(|entry| TemplateBuildLogEntry {
                timestamp: Some(entry.timestamp.into()),
                message: entry.message,
                level: entry.level.into(),
                fields: entry.fields.into_iter().collect(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> BuildLog {
        let log = BuildLog::default();
        let start = SystemTime::now() - Duration::from_secs(10);
        [
            (LogLevel::Debug, "debug"),
            (LogLevel::Info, "first"),
            (LogLevel::Error, "second"),
            (LogLevel::Info, "third"),
        ]
        .into_iter()
        .enumerate()
        .for_each(|(index, (level, message))| {
            log.entries.lock().push(LogEntry {
                timestamp: start + Duration::from_secs(index as u64),
                message: message.to_owned(),
                level,
                fields: BTreeMap::new(),
            })
        });
        log
    }

    fn messages(entries: Vec<TemplateBuildLogEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.message).collect()
    }

    #[test]
    fn filters_by_level_and_pages_with_offset_and_limit() {
        let query = LogQuery {
            level: LogLevel::Info as i32,
            offset: 1,
            limit: Some(1),
            ..LogQuery::default()
        };

        assert_eq!(messages(log().query(&query)), vec!["second"]);
        assert_eq!(messages(log().query(&LogQuery::default())).len(), 4);
    }

    #[test]
    fn backward_returns_the_newest_first_and_the_window_bounds_entries() {
        let backward = LogQuery {
            direction: LogsDirection::Backward,
            level: LogLevel::Info as i32,
            ..LogQuery::default()
        };
        assert_eq!(messages(log().query(&backward)), vec!["third", "second", "first"]);

        let ancient = LogQuery {
            end: Some(SystemTime::UNIX_EPOCH),
            ..LogQuery::default()
        };
        assert!(log().query(&ancient).is_empty());
    }

    #[test]
    fn an_unknown_level_returns_nothing_and_backward_keeps_ties_in_order() {
        let log = BuildLog::default();
        let at = SystemTime::now();
        ["a", "b", "c"].iter().for_each(|message| {
            log.entries.lock().push(LogEntry {
                timestamp: at,
                message: (*message).to_owned(),
                level: LogLevel::Info,
                fields: BTreeMap::new(),
            })
        });
        let unknown = LogQuery {
            level: 7,
            ..LogQuery::default()
        };
        let backward = LogQuery {
            direction: LogsDirection::Backward,
            ..LogQuery::default()
        };

        assert!(log.query(&unknown).is_empty());
        assert_eq!(messages(log.query(&backward)), ["a", "b", "c"]);
    }
}
