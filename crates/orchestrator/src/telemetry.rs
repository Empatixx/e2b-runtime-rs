pub mod endpoint;
pub mod fc;
pub mod grpc;
pub mod identity;
pub mod logs;
pub mod metrics;
pub mod report;
pub mod server;
pub mod traces;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opentelemetry::global;
use opentelemetry::propagation::TextMapCompositePropagator;
use opentelemetry::trace::TracerProvider;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::propagation::{BaggagePropagator, TraceContextPropagator};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tokio::runtime::{Handle, RuntimeFlavor};
use tracing::Subscriber;
use tracing_subscriber::filter::{FilterExt, LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt};

use endpoint::Endpoint;
use identity::Identity;
use logs::BaseFields;

const COLLECTOR_ENDPOINT_VARIABLE: &str = "OTEL_COLLECTOR_GRPC_ENDPOINT";
const TRACER_NAME: &str = "orchestrator";
const DEBUG_VARIABLE: &str = "E2B_DEBUG";
const OWN_TARGET: &str = "orchestrator";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct Providers {
    meters: SdkMeterProvider,
    traces: SdkTracerProvider,
    logs: SdkLoggerProvider,
}

pub struct Telemetry {
    providers: Option<Providers>,
    stopped: AtomicBool,
}

impl Telemetry {
    pub fn start(identity: &Identity) -> Result<Self, String> {
        Endpoint::from_env(COLLECTOR_ENDPOINT_VARIABLE)
            .map(|endpoint| providers(&endpoint, identity))
            .transpose()
            .map(|providers| {
                providers.iter().for_each(register);
                Self {
                    providers,
                    stopped: AtomicBool::new(false),
                }
            })
    }

    pub fn install_subscriber(&self) {
        tracing_subscriber::registry()
            .with(fmt::layer().with_filter(max_level()))
            .with(
                self.providers
                    .as_ref()
                    .map(|providers| trace_layer(&providers.traces).with_filter(own_events())),
            )
            .with(
                self.providers
                    .as_ref()
                    .map(|providers| log_layer(&providers.logs).with_filter(own_events())),
            )
            .init();
    }

    pub fn shutdown(&self) -> Result<(), String> {
        match self.stopped.swap(true, Ordering::SeqCst) {
            true => Ok(()),
            false => self.providers.as_ref().map_or(Ok(()), shut_down),
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        let shut = match Handle::try_current().map(|handle| handle.runtime_flavor()) {
            Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(|| self.shutdown()),
            _ => self.shutdown(),
        };
        if let Err(e) = shut {
            tracing::error!(error = %e, "error while shutting down telemetry");
        }
    }
}

fn shut_down(providers: &Providers) -> Result<(), String> {
    [
        providers.meters.shutdown_with_timeout(SHUTDOWN_TIMEOUT),
        providers.traces.shutdown_with_timeout(SHUTDOWN_TIMEOUT),
        providers.logs.shutdown_with_timeout(SHUTDOWN_TIMEOUT),
    ]
    .into_iter()
    .find_map(Result::err)
    .map_or(Ok(()), |e| Err(format!("telemetry shutdown: {e}")))
}

fn providers(endpoint: &Endpoint, identity: &Identity) -> Result<Providers, String> {
    let resource = identity.resource();
    Ok(Providers {
        meters: metrics::meter_provider(endpoint, resource.clone())?,
        traces: traces::tracer_provider(endpoint, resource.clone())?,
        logs: logs::logger_provider(endpoint, resource, BaseFields::current(&identity.service_name))?,
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

fn max_level() -> LevelFilter {
    match std::env::var(DEBUG_VARIABLE).is_ok_and(|debug| debug == "true") {
        true => LevelFilter::DEBUG,
        false => LevelFilter::INFO,
    }
}

fn own_events<S>() -> impl tracing_subscriber::layer::Filter<S> {
    Targets::new()
        .with_target(OWN_TARGET, LevelFilter::TRACE)
        .and(max_level())
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
    OpenTelemetryTracingBridge::new(provider)
}
