mod api;
mod checkpoint;
mod config;
mod firecracker;
mod host;
mod network;
mod proxy;
mod sandbox;
mod storage;
mod template;
mod volume;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

use config::{Config, ServeConfig};
use host::Host;
use sandbox::Sandboxes;
use template::BuildRequest;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve(ServeConfig),
    BuildTemplate(BuildRequest),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .init();
    let cli = Cli::parse();
    let host = Arc::new(Host::start(cli.config).await?);
    match cli.command {
        Command::Serve(serve_config) => serve(host, serve_config).await,
        Command::BuildTemplate(request) => build_template(host, request).await,
    }
}

async fn serve(host: Arc<Host>, config: ServeConfig) -> Result<()> {
    let sandboxes = Sandboxes::new(host.clone());
    sandboxes.spawn_reaper();
    let api = api::router(api::ApiState {
        host,
        sandboxes: sandboxes.clone(),
        api_key: config.api_key.into(),
        domain: config.domain.into(),
    });
    let api_listener = TcpListener::bind(&config.api_addr).await?;
    let proxy_listener = TcpListener::bind(&config.proxy_addr).await?;
    tracing::info!(api = config.api_addr, proxy = config.proxy_addr, "e2b-node listening");
    tokio::try_join!(
        axum::serve(api_listener, api),
        axum::serve(proxy_listener, proxy::router(sandboxes)),
    )?;
    Ok(())
}

async fn build_template(host: Arc<Host>, request: BuildRequest) -> Result<()> {
    let template = template::build(&host, &request).await?;
    println!(
        "built template {} (checkpoint {})",
        template.template_id, template.checkpoint
    );
    Ok(())
}
