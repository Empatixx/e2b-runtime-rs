#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), String> {
    serve::run().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("orchestrator {} runs on Linux only", orchestrator::version());
}

#[cfg(target_os = "linux")]
mod serve {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use orchestrator::server::app::App;
    use orchestrator::server::config::NodeConfig;
    use orchestrator::server::node::Node;
    use orchestrator::server::shutdown::{Drained, drain};
    use tokio::signal::unix::{Signal, SignalKind, signal};

    pub async fn run() -> Result<(), String> {
        tracing_subscriber::fmt().init();
        let config = NodeConfig::from_env()?;
        let node = Arc::new(Node::new(&config)?);
        node.sweep().await;
        let app = App::new(node.clone());
        app.start_background();
        let address = SocketAddr::from(([0, 0, 0, 0], config.grpc_port));
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|e| format!("listen on {address}: {e}"))?;
        tracing::info!(%address, node = %config.node_id, "orchestrator listening");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(
            axum::serve(listener, app.router())
                .with_graceful_shutdown(async move { drop(stopped.await) })
                .into_future(),
        );
        let mut signals = Signals::new()?;
        signals.next().await;
        tracing::info!("shutting down: draining sandboxes");
        node.status.shut_down();
        let drained = tokio::select! {
            () = tokio::time::sleep(config.drain_sleep) => drain(|| node.registry.is_empty(), signals.next()).await,
            () = signals.next() => Drained::Forced,
        };
        if drained == Drained::Forced {
            tracing::info!("forced shutdown: stopping every sandbox");
            node.shutdown().await;
        }
        drop(stop.send(()));
        let served = server.await.map_err(|e| e.to_string())?.map_err(|e| e.to_string());
        node.close().await;
        served
    }

    struct Signals {
        terminate: Signal,
    }

    impl Signals {
        fn new() -> Result<Self, String> {
            signal(SignalKind::terminate())
                .map(|terminate| Self { terminate })
                .map_err(|e| format!("listen for SIGTERM: {e}"))
        }

        async fn next(&mut self) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = self.terminate.recv() => {}
            }
        }
    }
}
