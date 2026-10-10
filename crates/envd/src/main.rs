use axum::Router;
use axum::serve::{Listener, ListenerExt};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use envd::cgroups::ProcessType;
use envd::cpus::sysfs::WRITE_ONLINE;
use envd::logs::layer::JsonLines;
use envd::port::forward::Forwarder;
use envd::reexec::{self, Helper};
use envd::upgrade::HandoverReport;
use envd::{app::AppState, config, host, router, upgrade};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const READ_BUFFER: usize = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(640);
const HANDOVER_FALLBACK_THAW: Duration = Duration::from_secs(60);

const HELPERS: &[Helper] = &[WRITE_ONLINE];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    reexec::main(HELPERS);
    run()
}

#[tokio::main]
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::parse(std::env::args()).inspect_err(|e| eprintln!("{e}"))?;
    if config.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if config.commit {
        println!("{}", option_env!("ENVD_COMMIT_SHA").unwrap_or_default());
        return Ok(());
    }

    let verbose = config.verbose;
    let port = config.port;
    let state = AppState::new(config);
    tracing_subscriber::registry()
        .with(JsonLines {
            exporter: state.log_exporter.clone(),
            stdout: verbose,
        })
        .init();

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await?
        .tap_io(|stream| {
            stream
                .set_nodelay(true)
                .unwrap_or_else(|e| tracing::warn!(error = %e, "failed to set TCP_NODELAY"))
        });
    tracing::info!(port, "envd listening");
    host::mmds::mark_sandbox(&state.env_vars, state.config.is_not_fc);
    if state.config.resume_handover {
        resume_handover(&state).await;
    }
    state.cpus.clone().start();
    if !state.config.is_not_fc {
        let poller = state.clone();
        tokio::spawn(async move {
            let opts = host::mmds::poll_for_opts(&poller.env_vars, None).await;
            poller.deliver_mmds(opts);
        });
    }
    let forwarder = Forwarder::new(
        state.freezer.manager().placement(ProcessType::Socat),
        state.forwards.clone(),
    );
    tokio::spawn(forwarder.run());
    serve(listener, router::build_router(state)).await
}

async fn resume_handover(state: &Arc<AppState>) {
    let resuming = state.clone();
    let report = match tokio::spawn(async move { upgrade::incoming::resume(&resuming).await }).await {
        Ok(Ok(report)) => report,
        Ok(Err(e)) => {
            eprintln!("resume-from-handover failed: {e}");
            failed_handover()
        }
        Err(panic) => {
            eprintln!("resume-from-handover panic (recovered): {panic}");
            let _ = state.freezer.unfreeze().await;
            failed_handover()
        }
    };
    *state.handover.lock() = Some(report);
    let fallback = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(HANDOVER_FALLBACK_THAW).await;
        if !fallback.initialized.load(Ordering::SeqCst) {
            eprintln!("envd: post-upgrade /init did not arrive within 1m0s; thawing workload as fallback");
            if let Err(e) = fallback.freezer.unfreeze().await {
                tracing::warn!(error = %e, "handover: unfreeze failed");
            }
        }
    });
}

fn failed_handover() -> HandoverReport {
    HandoverReport {
        failed: true,
        ..HandoverReport::default()
    }
}

async fn serve(mut listener: impl Listener, app: Router) -> ! {
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .max_buf_size(READ_BUFFER)
        .timer(TokioTimer::new())
        .header_read_timeout(IDLE_TIMEOUT);
    builder.http2().enable_connect_protocol();
    loop {
        let (stream, _) = listener.accept().await;
        let builder = builder.clone();
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            builder
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await
                .unwrap_or_else(|e| tracing::trace!(error = %e, "failed to serve connection"))
        });
    }
}
