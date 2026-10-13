use std::time::Duration;

use axum::Router;
use axum::serve::Listener;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;

const MAX_HEADER_BYTES: usize = (1 << 20) + 4096;
const MAX_HEADERS: usize = MAX_HEADER_BYTES / SHORTEST_HEADER_LINE.len();
const SHORTEST_HEADER_LINE: &[u8] = b"a:\r\n";
const IDLE_TIMEOUT: Duration = Duration::from_secs(640);

pub async fn serve(mut listener: impl Listener, app: Router) -> ! {
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
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await
                .unwrap_or_else(|e| tracing::trace!(error = %e, "failed to serve connection"))
        });
    }
}
