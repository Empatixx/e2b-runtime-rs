mod common;

use common::{args, spawn_server_with_state};

#[tokio::test]
async fn health_returns_204_no_store() {
    let (addr, _) = spawn_server_with_state().await;
    let res = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(res.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn phase2_routes_return_501() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    for p in ["/freeze", "/unfreeze", "/fsfreeze", "/fsthaw", "/collapse"] {
        let res = c.post(format!("http://{addr}{p}")).send().await.unwrap();
        assert_eq!(res.status(), 501, "{p}");
    }
    let res = c.get(format!("http://{addr}/metrics")).send().await.unwrap();
    assert_eq!(res.status(), 501);
}

#[test]
fn config_defaults() {
    let c = envd::config::parse(args(&["envd"])).unwrap();
    assert_eq!(c.port, 49983);
    assert_eq!(c.cgroup_root, "/sys/fs/cgroup");
    assert!(!c.is_not_fc && !c.no_cgroups && !c.verbose);
}

#[test]
fn config_accepts_single_and_double_dash() {
    let c = envd::config::parse(args(&["envd", "--port", "1234", "-isnotfc", "-no-cgroups=true", "-commit"])).unwrap();
    assert_eq!(c.port, 1234);
    assert!(c.is_not_fc && c.no_cgroups);
    assert!(c.commit);
}

#[test]
fn config_rejects_unknown_and_bad_values() {
    assert_eq!(envd::config::parse(args(&["envd", "-nope"])).unwrap_err(), "flag provided but not defined: -nope");
    assert!(envd::config::parse(args(&["envd", "-port", "x"])).is_err());
    assert_eq!(envd::config::parse(args(&["envd", "-port"])).unwrap_err(), "flag needs an argument: -port");
}
