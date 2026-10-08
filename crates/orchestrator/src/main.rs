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

    pub async fn run() -> Result<(), String> {
        tracing_subscriber::fmt().init();
        let config = NodeConfig::from_env()?;
        let node = Arc::new(Node::new(&config)?);
        let address = SocketAddr::from(([0, 0, 0, 0], config.grpc_port));
        tracing::info!(%address, node = %config.node_id, "orchestrator listening");
        tonic::transport::Server::builder()
            .add_service(SandboxServiceServer::new(SandboxServer::new(node)))
            .serve_with_shutdown(address, shutdown())
            .await
            .map_err(|e| e.to_string())
    }

    async fn shutdown() {
        let _ = tokio::signal::ctrl_c().await;
    }
}
