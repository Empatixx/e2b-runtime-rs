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

    use orchestrator::proto::sandbox_service_server::SandboxServiceServer;
    use orchestrator::server::config::NodeConfig;
    use orchestrator::server::node::Node;
    use orchestrator::server::service::SandboxServer;
    use tokio::signal::unix::{SignalKind, signal};

    pub async fn run() -> Result<(), String> {
        tracing_subscriber::fmt().init();
        let config = NodeConfig::from_env()?;
        let node = Arc::new(Node::new(&config)?);
        node.sweep().await;
        node.start_background();
        let address = SocketAddr::from(([0, 0, 0, 0], config.grpc_port));
        tracing::info!(%address, node = %config.node_id, "orchestrator listening");
        let served = tonic::transport::Server::builder()
            .add_service(SandboxServiceServer::new(SandboxServer::new(node.clone())))
            .serve_with_shutdown(address, shutdown_requested())
            .await
            .map_err(|e| e.to_string());
        tracing::info!("stopping every sandbox");
        node.shutdown().await;
        node.close().await;
        served
    }

    async fn shutdown_requested() {
        let terminate = async {
            match signal(SignalKind::terminate()) {
                Ok(mut terminate) => drop(terminate.recv().await),
                Err(_) => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = terminate => {}
        }
    }
}
