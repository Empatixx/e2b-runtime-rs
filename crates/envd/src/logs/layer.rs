use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use super::exporter::LogExporter;

pub struct JsonLines {
    pub exporter: Option<Arc<LogExporter>>,
    pub stdout: bool,
}

struct Fields {
    fields: Map<String, Value>,
    message: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, Value::from(format!("{value:?}")));
    }
}

impl Fields {
    fn record(&mut self, field: &Field, value: Value) {
        match field.name() {
            "message" => self.message = value.as_str().map(str::to_owned).or_else(|| Some(value.to_string())),
            name => {
                self.fields.insert(name.to_owned(), value);
            }
        }
    }
}

impl<S: Subscriber> Layer<S> for JsonLines {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if !event.metadata().target().starts_with("envd") {
            return;
        }
        let mut fields = Fields {
            fields: Map::new(),
            message: None,
        };
        event.record(&mut fields);
        let line = format!("{}\n", render(event.metadata().level(), fields));
        if let Some(exporter) = &self.exporter {
            exporter.write(line.as_bytes());
        }
        if self.stdout {
            print!("{line}");
        }
    }
}

fn render(level: &Level, fields: Fields) -> String {
    let mut object = Map::new();
    object.insert("level".to_owned(), Value::from(level_name(level)));
    object.insert("timestamp".to_owned(), Value::from(timestamp()));
    object.extend(fields.fields);
    if let Some(message) = fields.message {
        object.insert("message".to_owned(), Value::from(message));
    }
    serde_json::to_string(&object).unwrap_or_default()
}

fn level_name(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

fn timestamp() -> String {
    let full = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let (seconds, fraction) = full.trim_end_matches('Z').split_once('.').unwrap_or((&full, ""));
    match fraction.trim_end_matches('0') {
        "" => format!("{seconds}Z"),
        digits => format!("{seconds}.{digits}Z"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_use_zerolog_names() {
        assert_eq!(level_name(&Level::WARN), "warn");
        assert_eq!(level_name(&Level::ERROR), "error");
        assert!(timestamp().ends_with('Z'));
    }
}
