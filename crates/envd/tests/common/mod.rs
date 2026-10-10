#![allow(dead_code)]

use std::{net::SocketAddr, sync::Arc};

use envd::app::AppState;
use envd::cpus::{CpuManager, State};
use envd::reaper::Registry;
use parking_lot::Mutex;

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

#[derive(Default)]
pub struct FakeCpus {
    pub targets: Mutex<Vec<i64>>,
    pub state: Mutex<State>,
    pub refusal: Option<String>,
}

impl FakeCpus {
    pub fn with_state(online: usize, possible: usize) -> Self {
        Self {
            state: Mutex::new(State {
                online,
                possible,
                ..State::default()
            }),
            ..Self::default()
        }
    }

    pub fn recorded(&self) -> Vec<i64> {
        self.targets.lock().clone()
    }
}

impl CpuManager for FakeCpus {
    fn set_target(&self, count: i64) -> Result<(), String> {
        if let Some(refusal) = &self.refusal {
            return Err(refusal.clone());
        }
        self.targets.lock().push(count);
        self.state.lock().target = count as usize;
        Ok(())
    }

    fn status(&self) -> State {
        *self.state.lock()
    }

    fn start(self: Arc<Self>) {}
}

pub async fn spawn_server_with_cpus(cpus: Arc<FakeCpus>) -> SocketAddr {
    let cfg = envd::config::parse(args(&["envd", "-isnotfc", "-port", "0", "-no-cgroups"])).expect("valid flags");
    let state = AppState::with_cpus(cfg, Arc::new(Registry::default()), cpus);
    let app = envd::router::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    addr
}
