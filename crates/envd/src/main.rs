use axum::serve::ListenerExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use envd::cpus::sysfs::WRITE_ONLINE;
use envd::logs::layer::JsonLines;
use envd::port::forward::Forwarder;
use envd::reexec::{self, Helper};
use envd::upgrade::HandoverReport;
use envd::{app::AppState, config, connection, host, router, upgrade};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

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
    let forwarder = Forwarder::new(state.freezer.clone(), state.forwards.clone());
    tokio::spawn(forwarder.run());
    connection::serve(listener, router::build_router(state)).await
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
