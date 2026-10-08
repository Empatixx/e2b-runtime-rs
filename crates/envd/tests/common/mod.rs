#![allow(dead_code)]

use std::{net::SocketAddr, sync::Arc};

use envd::app::AppState;

pub fn args(list: &[&str]) -> impl Iterator<Item = String> {
    list.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
}

pub async fn spawn_server_with_state() -> (SocketAddr, Arc<AppState>) {
    spawn_server_with_flags(&["-no-cgroups"]).await
}

pub async fn spawn_server_with_flags(flags: &[&str]) -> (SocketAddr, Arc<AppState>) {
    let list: Vec<&str> = ["envd", "-isnotfc", "-port", "0"]
        .into_iter()
        .chain(flags.iter().copied())
        .collect();
    let cfg = envd::config::parse(args(&list)).expect("valid flags");
    let state = AppState::new(cfg);
    let app = envd::router::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (addr, state)
}
