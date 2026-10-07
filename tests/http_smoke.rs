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
    let c = envd::config::parse(args(&[
        "envd",
        "--port",
        "1234",
        "-isnotfc",
        "-no-cgroups=true",
        "-commit",
    ]))
    .unwrap();
    assert_eq!(c.port, 1234);
    assert!(c.is_not_fc && c.no_cgroups);
    assert!(c.commit);
}

#[test]
fn config_rejects_unknown_and_bad_values() {
    assert_eq!(
        envd::config::parse(args(&["envd", "-nope"])).unwrap_err(),
        "flag provided but not defined: -nope"
    );
    assert!(envd::config::parse(args(&["envd", "-port", "x"])).is_err());
    assert_eq!(
        envd::config::parse(args(&["envd", "-port"])).unwrap_err(),
        "flag needs an argument: -port"
    );
}

#[tokio::test]
async fn token_required_except_excluded_paths() {
    let (addr, state) = spawn_server_with_state().await;
    *state.access_token.write() = Some("t".into());
    let c = reqwest::Client::new();

    assert_eq!(
        c.get(format!("http://{addr}/health")).send().await.unwrap().status(),
        204
    );

    let res = c.get(format!("http://{addr}/envs")).send().await.unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(res.headers()["content-type"], "application/json; charset=utf-8");
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(
        body["message"],
        "unauthorized access, please provide a valid access token or method signing if supported"
    );
    assert_eq!(body["code"], 401);

    let res = c
        .get(format!("http://{addr}/envs"))
        .header("X-Access-Token", "t")
        .send()
        .await
        .unwrap();
    assert_ne!(res.status(), 401);
}

#[tokio::test]
async fn no_token_set_means_open() {
    let (addr, _) = spawn_server_with_state().await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/envs"))
        .send()
        .await
        .unwrap();
    assert_ne!(res.status(), 401);
}

#[tokio::test]
async fn token_check_runs_before_basic_user_lookup() {
    let (addr, state) = spawn_server_with_state().await;
    *state.access_token.write() = Some("t".into());
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/envs"))
        .basic_auth("definitely-missing-user", Some(""))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({ "code": 401, "message": "unauthorized access, please provide a valid access token or method signing if supported" })
    );
}
