#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = envd::config::parse(std::env::args()).inspect_err(|e| eprintln!("{e}"))?;
    if cfg.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(if cfg.verbose { "debug" } else { "info" })
        .init();

    let port = cfg.port;
    let app = envd::server::build_router(envd::state::AppState::new(cfg));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "envd listening");
    axum::serve(listener, app).await?;
    Ok(())
}
