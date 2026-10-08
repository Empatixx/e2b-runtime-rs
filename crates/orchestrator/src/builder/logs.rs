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
    pub level: LogLevel,
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

impl BuildLog {
    pub fn push(&self, level: LogLevel, message: impl Into<String>) {
        self.entries.lock().push(LogEntry {
            timestamp: SystemTime::now(),
            message: message.into(),
            level,
            fields: BTreeMap::new(),
        });
    }

    pub fn query(&self, query: &LogQuery) -> Vec<TemplateBuildLogEntry> {
        let now = SystemTime::now();
        let start = query.start.unwrap_or(now - DEFAULT_TIME_RANGE);
        let end = query.end.unwrap_or(now);
        let limit = query
            .limit
            .map_or(MAX_ENTRIES_PER_REQUEST, |limit| limit.min(MAX_ENTRIES_PER_REQUEST));
        let mut entries = self.entries.lock().clone();
        entries.sort_by_key(|entry| entry.timestamp);
        if query.direction == LogsDirection::Backward {
            entries.reverse();
        }
        entries
            .into_iter()
            .filter(|entry| entry.level >= query.level)
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
        log.push(LogLevel::Debug, "debug");
        log.push(LogLevel::Info, "first");
        log.push(LogLevel::Error, "second");
        log.push(LogLevel::Info, "third");
        log
    }

    fn messages(entries: Vec<TemplateBuildLogEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.message).collect()
    }

    #[test]
    fn filters_by_level_and_pages_with_offset_and_limit() {
        let query = LogQuery {
            level: LogLevel::Info,
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
            level: LogLevel::Info,
            ..LogQuery::default()
        };
        assert_eq!(messages(log().query(&backward)), vec!["third", "second", "first"]);

        let ancient = LogQuery {
            end: Some(SystemTime::UNIX_EPOCH),
            ..LogQuery::default()
        };
        assert!(log().query(&ancient).is_empty());
    }
}
