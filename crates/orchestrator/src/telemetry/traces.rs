use opentelemetry_otlp::{Compression, SpanExporter, WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

use super::endpoint::Endpoint;

pub fn tracer_provider(endpoint: &Endpoint, resource: Resource) -> Result<SdkTracerProvider, String> {
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.url())
        .with_compression(Compression::Gzip)
        .build()
        .map_err(|e| format!("failed to create trace exporter: {e}"))?;
    Ok(SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_resource(resource)
        .with_span_processor(BatchSpanProcessor::builder(exporter, Tokio).build())
        .build())
}
