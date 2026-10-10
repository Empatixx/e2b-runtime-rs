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
async fn collapse_reports_its_work_like_upstream() {
    let (addr, _) = spawn_server_with_state().await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/collapse"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["cache-control"], "no-store");
    let body = res.text().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let keys: Vec<&str> = parsed.as_object().unwrap().keys().map(String::as_str).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        ["alreadyHuge", "chunks", "collapsed", "elapsedMs", "regions", "skipped"]
    );
    assert!(body.starts_with("{\"alreadyHuge\":") && body.ends_with("}\n"), "{body}");
}

#[tokio::test]
async fn metrics_report_the_guest_like_upstream() {
    let (addr, _) = spawn_server_with_state().await;
    let res = reqwest::get(format!("http://{addr}/metrics")).await.unwrap();
    let cache = res.headers()["cache-control"].to_str().unwrap().to_owned();
    let content_type = res.headers()["content-type"].to_str().unwrap().to_owned();
    let status = res.status();
    let body = res.text().await.unwrap();

    assert_eq!(
        (cache.as_str(), content_type.as_str()),
        ("no-store", "application/json")
    );
    match cfg!(target_os = "linux") {
        true => {
            assert_eq!(status, 200);
            assert!(body.ends_with("}\n"), "{body:?}");
            let metrics: serde_json::Value = serde_json::from_str(&body).unwrap();
            let mut keys: Vec<&str> = metrics.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "cpu_count",
                    "cpu_possible",
                    "cpu_target",
                    "cpu_target_attempts",
                    "cpu_used_pct",
                    "cpu_write_pending_ms",
                    "disk_total",
                    "disk_used",
                    "mem_cache",
                    "mem_total",
                    "mem_used",
                    "ts"
                ]
            );
            assert!(metrics["cpu_count"].as_u64().unwrap() > 0);
            assert!(metrics["mem_used"].as_u64().unwrap() <= metrics["mem_total"].as_u64().unwrap());
        }
        false => assert_eq!((status.as_u16(), body.as_str()), (500, "")),
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn metrics_carry_the_cpu_target() {
    let cpus = std::sync::Arc::new(common::FakeCpus::with_state(8, 16));
    {
        let mut state = cpus.state.lock();
        state.target = 8;
        state.attempts = 2;
        state.write_pending = std::time::Duration::from_millis(1500);
    }
    let addr = common::spawn_server_with_cpus(cpus).await;

    let res = reqwest::get(format!("http://{addr}/metrics")).await.unwrap();

    assert_eq!(res.status(), 200);
    let metrics: serde_json::Value = res.json().await.unwrap();
    assert_eq!(metrics["cpu_possible"], 16);
    assert_eq!(metrics["cpu_target"], 8);
    assert_eq!(metrics["cpu_target_attempts"], 2);
    assert_eq!(metrics["cpu_write_pending_ms"], 1500);
    assert!(metrics.get("cpu_count").is_some());
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

#[tokio::test]
async fn freeze_and_unfreeze_answer_like_upstream() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let post = |path: &str| c.post(format!("http://{addr}{path}")).send();

    let bare = post("/freeze").await.unwrap();
    assert_eq!(bare.status(), 204);

    let reported = post("/freeze?maxWaitMs=20&mode=hierarchy").await.unwrap();
    assert_eq!(reported.status(), 200);
    assert_eq!(reported.headers()["cache-control"], "no-store");
    assert_eq!(reported.headers()["content-type"], "application/json");
    let body = reported.text().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        body.starts_with("{\"allowlisted\":0,\"failed\":0,\"frozen\":0,\"mode\":\"legacy\",\"notFrozen\":0,"),
        "{body}"
    );
    assert!(body.ends_with("}\n"));
    assert_eq!(
        (parsed["requested"].as_i64(), parsed["unobservable"].as_i64()),
        (Some(2), Some(2))
    );

    let invalid = post("/freeze?maxWaitMs=soon").await.unwrap();
    assert_eq!(invalid.status(), 400);
    assert_eq!(
        invalid.text().await.unwrap(),
        "Invalid format for parameter maxWaitMs: error binding string parameter: strconv.ParseInt: parsing \"soon\": invalid syntax\n"
    );

    let thawed = post("/unfreeze").await.unwrap();
    assert_eq!(thawed.status(), 204);
    assert_eq!(thawed.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn cors_answers_like_rs_cors() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();

    let preflight = c
        .request(reqwest::Method::OPTIONS, format!("http://{addr}/envs"))
        .header("origin", "https://app.example")
        .header("access-control-request-method", "get")
        .header("access-control-request-headers", "x-access-token, content-type")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), 204);
    let headers = preflight.headers();
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(headers["access-control-allow-methods"], "GET");
    assert_eq!(headers["access-control-allow-headers"], "x-access-token, content-type");
    assert_eq!(headers["access-control-max-age"], "7200");
    assert_eq!(headers.get_all("vary").iter().count(), 3);

    let refused = c
        .request(reqwest::Method::OPTIONS, format!("http://{addr}/envs"))
        .header("origin", "https://app.example")
        .header("access-control-request-method", "TRACE")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 204);
    assert!(!refused.headers().contains_key("access-control-allow-origin"));

    let actual = c
        .get(format!("http://{addr}/health"))
        .header("origin", "https://app.example")
        .send()
        .await
        .unwrap();
    assert_eq!(actual.headers()["access-control-allow-origin"], "*");
    assert_eq!(
        actual.headers()["access-control-expose-headers"],
        "Grpc-Status, Grpc-Message, Grpc-Status-Details-Bin, Location, Cache-Control, X-Content-Type-Options"
    );
    assert_eq!(actual.headers()["vary"], "Origin");

    let no_origin = c.get(format!("http://{addr}/health")).send().await.unwrap();
    assert!(!no_origin.headers().contains_key("access-control-allow-origin"));
}
