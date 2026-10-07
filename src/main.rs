use axum::serve::ListenerExt;
use envd::{app::AppState, config, router};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::parse(std::env::args()).inspect_err(|e| eprintln!("{e}"))?;
    if config.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
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
    axum::serve(listener, router::build_router(AppState::new(config))).await?;
    Ok(())
}
