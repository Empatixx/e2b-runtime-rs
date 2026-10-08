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
    let now = chrono::Utc::now();
    let newer = now.to_rfc3339();
    let older = (now - chrono::Duration::hours(1)).to_rfc3339();
    assert_eq!(init(&newer, "newer").await.unwrap().status(), 204);
    assert_eq!(init(&older, "older").await.unwrap().status(), 204);
    assert_eq!(state.defaults().user, "newer");
    assert_eq!(init(&newer, "same").await.unwrap().status(), 204);
    assert_eq!(state.defaults().user, "same");
}

#[tokio::test]
async fn init_reports_effective_defaults() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let init = |body: serde_json::Value| c.post(format!("http://{addr}/init")).json(&body).send();

    let res = init(serde_json::json!({})).await.unwrap();
    assert_eq!(res.headers()["x-envd-defaults"], r#"{"user":"root","fallback":true}"#);

    let res = init(serde_json::json!({ "defaultUser": "root", "defaultWorkdir": "/tmp" }))
        .await
        .unwrap();
    assert_eq!(
        res.headers()["x-envd-defaults"],
        r#"{"user":"root","workdir":"/tmp","fallback":false}"#
    );
}

#[tokio::test]
async fn init_rejects_empty_or_escaped_tokens_and_reports_memory() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let post = |body: &'static str| {
        c.post(format!("http://{addr}/init"))
            .header("content-type", "application/json")
            .body(body)
            .send()
    };

    let empty = post(r#"{"accessToken":""}"#).await.unwrap();
    let escaped = post(r#"{"accessToken":"a\u0062"}"#).await.unwrap();
    let mixed_case = post(r#"{"DEFAULTUSER":"user"}"#).await.unwrap();

    assert_eq!(empty.status(), 400);
    assert_eq!(escaped.status(), 400);
    assert!(empty.headers().contains_key("x-envd-version"));
    assert_eq!(
        empty.headers()["x-envd-memory"],
        r#"{"request":0,"low":0,"floor":0,"partial":true}"#
    );
    assert_eq!(mixed_case.status(), 204);
    assert_eq!(
        mixed_case.headers()["x-envd-defaults"],
        r#"{"user":"user","fallback":false}"#
    );
}
