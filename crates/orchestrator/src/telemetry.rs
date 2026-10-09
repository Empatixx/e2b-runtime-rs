pub mod endpoint;
pub mod identity;
pub mod logs;
pub mod metrics;
pub mod server;
pub mod traces;

use opentelemetry::global;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry::trace::TracerProvider;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Subscriber;
use tracing_subscriber::filter::{LevelFilter, filter_fn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt};

use endpoint::Endpoint;
use identity::Identity;

const COLLECTOR_ENDPOINT_VARIABLE: &str = "OTEL_COLLECTOR_GRPC_ENDPOINT";
const TRACER_NAME: &str = "orchestrator";
const EXPORTER_TARGETS: [&str; 7] = [
    "opentelemetry",
    "tonic",
    "tower",
    "h2",
    "hyper",
    "reqwest",
    "tracing_opentelemetry",
];

struct Providers {
    meters: SdkMeterProvider,
    traces: SdkTracerProvider,
    logs: SdkLoggerProvider,
}

pub struct Telemetry {
    providers: Option<Providers>,
}

impl Telemetry {
    pub fn start(identity: &Identity) -> Result<Self, String> {
        Endpoint::from_env(COLLECTOR_ENDPOINT_VARIABLE)
            .map(|endpoint| providers(&endpoint, identity))
            .transpose()
            .map(|providers| {
                providers.iter().for_each(register);
                Self { providers }
            })
    }

    pub fn install_subscriber(&self) {
        tracing_subscriber::registry()
            .with(fmt::layer().with_filter(LevelFilter::INFO))
            .with(self.providers.as_ref().map(|providers| trace_layer(&providers.traces)))
            .with(self.providers.as_ref().map(|providers| log_layer(&providers.logs)))
            .init();
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.providers.as_ref().map_or(Ok(()), |providers| {
            let flushed = [
                providers.meters.force_flush(),
                providers.traces.force_flush(),
                providers.logs.force_flush(),
            ];
            let shut = [
                providers.meters.shutdown(),
                providers.traces.shutdown(),
                providers.logs.shutdown(),
            ];
            flushed
                .into_iter()
                .chain(shut)
                .find_map(Result::err)
                .map_or(Ok(()), |e| Err(format!("telemetry shutdown: {e}")))
        })
    }
}

fn providers(endpoint: &Endpoint, identity: &Identity) -> Result<Providers, String> {
    let resource = identity.resource();
    Ok(Providers {
        meters: metrics::meter_provider(endpoint, resource.clone())?,
        traces: traces::tracer_provider(endpoint, resource.clone())?,
        logs: logs::logger_provider(endpoint, resource)?,
    })
}

fn register(providers: &Providers) {
    global::set_meter_provider(providers.meters.clone());
    global::set_tracer_provider(providers.traces.clone());
    global::set_text_map_propagator(TextMapCompositePropagator::new(vec![
        Box::new(TraceContextPropagator::new()),
        Box::new(BaggagePropagator::new()),
    ]));
}

fn trace_layer<S>(provider: &SdkTracerProvider) -> impl Layer<S> + use<S>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    tracing_opentelemetry::layer().with_tracer(provider.tracer(TRACER_NAME))
}

fn log_layer<S>(provider: &SdkLoggerProvider) -> impl Layer<S> + use<S>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    OpenTelemetryTracingBridge::new(provider).with_filter(filter_fn(|metadata| {
        !EXPORTER_TARGETS
            .iter()
            .any(|target| metadata.target().starts_with(target))
    }))
}
