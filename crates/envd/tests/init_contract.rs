mod common;

use common::{SteppingClock, serve_state, spawn_server_with_state, state_with_clock};

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
    assert_eq!(res.headers()["x-envd-version"], "0.9.0");
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
    let state = state_with_clock(SteppingClock::new(std::time::Duration::ZERO));
    let addr = serve_state(state.clone()).await;
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

#[tokio::test]
async fn init_accepts_bodies_over_two_megabytes() {
    let (addr, _) = spawn_server_with_state().await;
    let big = "x".repeat(3 * 1024 * 1024);
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/init"))
        .json(&serde_json::json!({ "envVars": {"BIG": big} }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
}

mod cpu_count {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::common::{FakeCpus, spawn_server_with_cpus};

    async fn post_init(addr: std::net::SocketAddr, body: Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("http://{addr}/init"))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    fn cpus_header(response: &reqwest::Response) -> Value {
        serde_json::from_str(response.headers()["x-envd-cpus"].to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn records_the_target_and_reports_it_on_the_header() {
        let cpus = Arc::new(FakeCpus::with_state(4, 16));
        let addr = spawn_server_with_cpus(cpus.clone()).await;

        let response = post_init(addr, json!({ "cpuCount": 8 })).await;

        assert_eq!(response.status(), 204);
        assert_eq!(cpus.recorded(), vec![8]);
        assert_eq!(
            cpus_header(&response),
            json!({ "online": 4, "possible": 16, "target": 8 })
        );
    }

    #[tokio::test]
    async fn without_cpu_count_the_target_is_untouched_but_the_state_is_still_reported() {
        let cpus = Arc::new(FakeCpus::with_state(4, 16));
        let addr = spawn_server_with_cpus(cpus.clone()).await;

        let response = post_init(addr, json!({})).await;

        assert_eq!(response.status(), 204);
        assert!(cpus.recorded().is_empty());
        assert_eq!(
            cpus_header(&response),
            json!({ "online": 4, "possible": 16, "target": 0 })
        );
    }

    #[tokio::test]
    async fn a_rejected_count_does_not_fail_the_init_and_is_named_on_the_header() {
        let cpus = Arc::new(FakeCpus {
            refusal: Some("cpu target 32 outside 1..16".to_owned()),
            ..FakeCpus::with_state(4, 16)
        });
        let addr = spawn_server_with_cpus(cpus.clone()).await;

        let response = post_init(addr, json!({ "cpuCount": 32 })).await;

        assert_eq!(response.status(), 204);
        assert!(cpus.recorded().is_empty());
        assert_eq!(cpus_header(&response)["rejected"], "cpu target 32 outside 1..16");

        let next = post_init(addr, json!({})).await;
        assert!(cpus_header(&next).get("rejected").is_none());
    }

    #[tokio::test]
    async fn a_stale_request_does_not_move_the_target() {
        let cpus = Arc::new(FakeCpus::with_state(4, 16));
        let addr = spawn_server_with_cpus(cpus.clone()).await;
        let now = chrono::Utc::now();
        let minute_ago = now - chrono::Duration::minutes(1);

        let first = post_init(addr, json!({ "cpuCount": 8, "timestamp": now.to_rfc3339() })).await;
        assert_eq!(first.status(), 204);
        let stale = post_init(addr, json!({ "cpuCount": 2, "timestamp": minute_ago.to_rfc3339() })).await;
        assert_eq!(stale.status(), 204);

        assert_eq!(cpus.recorded(), vec![8]);
    }

    #[tokio::test]
    async fn the_header_carries_only_the_counts() {
        let cpus = Arc::new(FakeCpus::with_state(6, 16));
        {
            let mut state = cpus.state.lock();
            state.target = 8;
            state.attempts = 3;
        }
        let addr = spawn_server_with_cpus(cpus).await;

        let response = post_init(addr, json!({})).await;

        assert_eq!(response.status(), 204);
        assert_eq!(
            cpus_header(&response),
            json!({ "online": 6, "possible": 16, "target": 8 })
        );
    }

    #[tokio::test]
    async fn a_non_integer_count_is_a_bad_request() {
        let addr = spawn_server_with_cpus(Arc::new(FakeCpus::with_state(4, 16))).await;

        let response = post_init(addr, json!({ "cpuCount": 8.5 })).await;

        assert_eq!(response.status(), 400);
    }
}
