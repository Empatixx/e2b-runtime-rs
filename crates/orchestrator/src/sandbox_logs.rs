mod line;
mod writer;

use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use serde_json::Value;

use self::line::LogLine;
use self::writer::LogWriter;
use crate::featureflags::FeatureFlags;

static EXTERNAL: OnceLock<ExternalLogs> = OnceLock::new();

struct ExternalLogs {
    writer: Option<Arc<LogWriter>>,
    service: String,
}

pub fn install_external(collector: &str, flags: FeatureFlags, service: &str) {
    let writer = (!collector.is_empty()).then(|| LogWriter::new(collector, flags));
    let _ = EXTERNAL.set(ExternalLogs {
        writer,
        service: service.to_owned(),
    });
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxMeta {
    pub sandbox_id: String,
    pub template_id: String,
    pub team_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Healthcheck {
    Success,
    Fail,
    ReportSuccess,
    ReportFail,
}

pub type Fields = Vec<(&'static str, Value)>;

impl SandboxMeta {
    pub fn of(config: &crate::proto::SandboxConfig) -> Self {
        Self {
            sandbox_id: config.sandbox_id.clone(),
            template_id: config.template_id.clone(),
            team_id: config.team_id.clone(),
        }
    }
}

pub struct SandboxLog<'a> {
    meta: &'a SandboxMeta,
}

pub fn external(meta: &SandboxMeta) -> SandboxLog<'_> {
    SandboxLog { meta }
}

impl SandboxLog<'_> {
    pub fn info(&self, message: &str, fields: Fields) {
        self.write(Level::Info, message, fields);
    }

    pub fn warn(&self, message: &str, fields: Fields) {
        self.write(Level::Warn, message, fields);
    }

    pub fn error(&self, message: &str, fields: Fields) {
        self.write(Level::Error, message, fields);
    }

    pub fn healthcheck(&self, action: Healthcheck) {
        let (level, message, healthy) = match action {
            Healthcheck::Success => (Level::Info, "Sandbox healthcheck recovered", true),
            Healthcheck::Fail => (Level::Error, "Sandbox healthcheck started failing", false),
            Healthcheck::ReportSuccess => (Level::Info, "Control sandbox healthcheck was successful", true),
            Healthcheck::ReportFail => (Level::Error, "Control sandbox healthcheck was unsuccessful", false),
        };
        self.write(level, message, vec![("healthcheck", Value::Bool(healthy))]);
    }

    fn write(&self, level: Level, message: &str, fields: Fields) {
        let Some(logs) = EXTERNAL.get() else {
            return;
        };
        match &logs.writer {
            Some(writer) => writer.write(self.line(&logs.service, level, message, fields).encode()),
            None => self.trace(level, message, &fields),
        }
    }

    fn line<'b>(&'b self, service: &'b str, level: Level, message: &'b str, fields: Fields) -> LogLine<'b> {
        LogLine {
            level: level.label(),
            time: SystemTime::now(),
            message,
            service,
            meta: self.meta,
            fields,
        }
    }

    fn trace(&self, level: Level, message: &str, fields: &Fields) {
        let fields = Value::Object(
            fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        );
        let sandbox_id = &self.meta.sandbox_id;
        match level {
            Level::Info => {
                tracing::info!(sandbox.id = %sandbox_id, fields = %fields, "{message}")
            }
            Level::Warn => {
                tracing::warn!(sandbox.id = %sandbox_id, fields = %fields, "{message}")
            }
            Level::Error => {
                tracing::error!(sandbox.id = %sandbox_id, fields = %fields, "{message}")
            }
        }
    }
}
