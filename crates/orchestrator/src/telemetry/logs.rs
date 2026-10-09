use opentelemetry_otlp::{Compression, LogExporter, WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor;
use opentelemetry_sdk::runtime::Tokio;

use super::endpoint::Endpoint;

pub fn logger_provider(endpoint: &Endpoint, resource: Resource) -> Result<SdkLoggerProvider, String> {
    let exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.url())
        .with_compression(Compression::Gzip)
        .build()
        .map_err(|e| format!("failed to create logs exporter: {e}"))?;
    Ok(SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_log_processor(BatchLogProcessor::builder(exporter, Tokio).build())
        .build())
}
