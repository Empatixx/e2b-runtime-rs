use std::future::Future;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;

const KEEPALIVE_TIME: Duration = Duration::from_secs(15);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn serve(listener: TcpListener, router: Router, shutdown: impl Future<Output = ()>) {
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Some(KEEPALIVE_TIME))
        .keep_alive_timeout(KEEPALIVE_TIMEOUT);
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = &mut shutdown => break,
        };
        let Ok((stream, _)) = accepted else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let connection = builder
            .serve_connection_with_upgrades(TokioIo::new(stream), TowerToHyperService::new(router.clone()))
            .into_owned();
        tokio::spawn(graceful.watch(connection));
    }
    graceful.shutdown().await;
}

#[cfg(test)]
mod tests {
    use axum::routing::get;

    use super::*;

    #[tokio::test]
    async fn requests_are_served_until_the_shutdown() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            Router::new().route("/", get(|| async { "ok" })),
            async move { drop(stopped.await) },
        ));

        let body = reqwest::get(format!("http://{address}/"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();

        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn http2_connections_with_keepalive_are_served() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            Router::new().route("/", get(|| async { "ok" })),
            async move { drop(stopped.await) },
        ));
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build_http::<String>();

        let response = client.get(format!("http://{address}/").parse().unwrap()).await.unwrap();
        stop.send(()).unwrap();
        drop(client);
        server.await.unwrap();

        assert_eq!(response.version(), hyper::Version::HTTP_2);
        assert_eq!(response.status(), hyper::StatusCode::OK);
    }
}
