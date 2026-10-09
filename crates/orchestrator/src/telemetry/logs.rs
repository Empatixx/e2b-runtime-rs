use opentelemetry::logs::{AnyValue, LogRecord};
use opentelemetry::{InstrumentationScope, Key};
use opentelemetry_otlp::{Compression, LogExporter, WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord, SdkLoggerProvider};
use opentelemetry_sdk::runtime::Tokio;

use super::endpoint::Endpoint;

#[derive(Debug, Clone, PartialEq)]
pub struct BaseFields {
    pub service: String,
    pub internal: bool,
    pub pid: u32,
}

impl BaseFields {
    pub fn current(service: &str) -> Self {
        Self {
            service: service.to_owned(),
            internal: true,
            pid: std::process::id(),
        }
    }

    fn entries(&self) -> [(&'static str, AnyValue); 3] {
        [
            ("service", AnyValue::from(self.service.clone())),
            ("internal", AnyValue::from(self.internal)),
            ("pid", AnyValue::from(i64::from(self.pid))),
        ]
    }
}

impl LogProcessor for BaseFields {
    fn emit(&self, record: &mut SdkLogRecord, _scope: &InstrumentationScope) {
        self.entries()
            .into_iter()
            .for_each(|(key, value)| record.add_attribute(Key::new(key), value));
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }
}

pub fn logger_provider(endpoint: &Endpoint, resource: Resource, base: BaseFields) -> Result<SdkLoggerProvider, String> {
    let exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.url())
        .with_compression(Compression::Gzip)
        .build()
        .map_err(|e| format!("failed to create logs exporter: {e}"))?;
    Ok(SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_log_processor(base)
        .with_log_processor(BatchLogProcessor::builder(exporter, Tokio).build())
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_record_carries_the_service_internal_and_pid_fields_of_upstream() {
        let base = BaseFields {
            service: "orchestrator".to_owned(),
            internal: true,
            pid: 7,
        };

        let keys: Vec<_> = base.entries().into_iter().map(|(key, _)| key).collect();

        assert_eq!(keys, ["service", "internal", "pid"]);
    }
}
