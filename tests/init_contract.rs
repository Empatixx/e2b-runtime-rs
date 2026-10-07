mod common;

use common::spawn_server_with_state;

#[tokio::test]
async fn init_sets_envs_user_and_token() {
    let (addr, state) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let res = c
        .post(format!("http://{addr}/init"))
        .json(&serde_json::json!({
            "envVars": {"FOO": "bar"}, "accessToken": "t1", "defaultUser": "user", "defaultWorkdir": "/tmp"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert!(res.headers().contains_key("x-envd-version"));
    assert_eq!(state.defaults().user, "user");
    assert_eq!(state.defaults().workdir.as_deref(), Some("/tmp"));

    let envs: serde_json::Value = c
        .get(format!("http://{addr}/envs"))
        .header("X-Access-Token", "t1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(envs["FOO"], "bar");
}

#[tokio::test]
async fn init_token_rotation_rules() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let post = |tok: &'static str| {
        c.post(format!("http://{addr}/init"))
            .json(&serde_json::json!({ "accessToken": tok }))
            .send()
    };
    assert_eq!(post("t1").await.unwrap().status(), 204, "first init sets token");
    assert_eq!(post("t1").await.unwrap().status(), 204, "same token again ok");
    let res = post("t2").await.unwrap();
    assert_eq!(res.status(), 401, "different token without MMDS rejected");
    assert_eq!(res.text().await.unwrap(), "access token validation failed");
}

#[tokio::test]
async fn init_rejects_bad_json_and_older_timestamp_keeps_data() {
    let (addr, state) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let res = c
        .post(format!("http://{addr}/init"))
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    let init = |ts: &str, user: &str| {
        c.post(format!("http://{addr}/init"))
            .json(&serde_json::json!({ "timestamp": ts, "defaultUser": user }))
            .send()
    };
    assert_eq!(init("2026-10-05T10:00:00Z", "newer").await.unwrap().status(), 204);
    assert_eq!(init("2026-10-05T09:00:00Z", "older").await.unwrap().status(), 204);
    assert_eq!(state.defaults().user, "newer");
}
