use axum::Router;
use axum::serve::{Listener, ListenerExt};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use envd::connection::{GoErrorPages, MAX_HEADER_BYTES, MAX_HEADERS};
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
    let forwarder = Forwarder::new(state.freezer.clone(), state.forwards.clone());
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
        .max_buf_size(MAX_HEADER_BYTES)
        .max_headers(MAX_HEADERS)
        .timer(TokioTimer::new())
        .header_read_timeout(IDLE_TIMEOUT);
    builder.http2().enable_connect_protocol();
    loop {
        let (stream, _) = listener.accept().await;
        let builder = builder.clone();
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            builder
                .serve_connection_with_upgrades(TokioIo::new(GoErrorPages::new(stream)), service)
                .await
                .unwrap_or_else(|e| tracing::trace!(error = %e, "failed to serve connection"))
        });
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    async fn response_to(headers: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, Router::new().route("/", get(|| async { "ok" }))));
        let mut stream = TcpStream::connect(address).await.unwrap();
        let request = format!("GET / HTTP/1.1\r\nHost: x\r\n{headers}Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap_or_default();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap_or_default();
        String::from_utf8_lossy(&response).into_owned()
    }

    async fn status_for_header_of(size: usize) -> String {
        response_to(format!("X-Big: {}\r\n", "a".repeat(size)))
            .await
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    #[tokio::test]
    async fn headers_up_to_a_mebibyte_are_accepted_like_go() {
        assert_eq!(status_for_header_of(1 << 20).await, "HTTP/1.1 200 OK");
    }

    #[tokio::test]
    async fn headers_over_the_go_limit_get_the_go_page() {
        assert_eq!(
            response_to(format!("X-Big: {}\r\n", "a".repeat(2 << 20))).await,
            "HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n431 Request Header Fields Too Large"
        );
    }

    #[tokio::test]
    async fn thousands_of_headers_are_accepted_like_go() {
        let headers = (0..5000).map(|i| format!("X-H{i}: v\r\n")).collect::<String>();
        assert!(response_to(headers).await.starts_with("HTTP/1.1 200 OK"));
    }
}
