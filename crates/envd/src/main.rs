use axum::Router;
use axum::serve::{Listener, ListenerExt};
use std::time::Duration;

use envd::cgroups::ProcessType;
use envd::port::forward::Forwarder;
use envd::{app::AppState, config, host, router};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;

const READ_BUFFER: usize = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(640);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::parse(std::env::args()).inspect_err(|e| eprintln!("{e}"))?;
    if config.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if config.commit {
        println!("{}", option_env!("ENVD_COMMIT_SHA").unwrap_or_default());
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(if config.verbose { "debug" } else { "info" })
        .init();

    let port = config.port;
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await?
        .tap_io(|stream| {
            stream
                .set_nodelay(true)
                .unwrap_or_else(|e| tracing::warn!(error = %e, "failed to set TCP_NODELAY"))
        });
    tracing::info!(port, "envd listening");
    let state = AppState::new(config);
    host::mmds::mark_sandbox(&state.env_vars, state.config.is_not_fc);
    if !state.config.is_not_fc {
        let poller = state.clone();
        tokio::spawn(async move { host::mmds::poll_for_opts(&poller.env_vars, None).await });
    }
    let forwarder = Forwarder::new(state.freezer.manager().placement(ProcessType::Socat));
    tokio::spawn(forwarder.run());
    serve(listener, router::build_router(state)).await
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
