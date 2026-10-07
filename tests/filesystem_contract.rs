mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::spawn_server_with_state;
use serde_json::{Value, json};

fn current_user() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap()
        .name
}

async fn call(addr: std::net::SocketAddr, method: &str, user: &str, body: Value) -> (u16, Value) {
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/filesystem.Filesystem/{method}"))
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Basic {}", STANDARD.encode(format!("{user}:"))),
        )
        .json(&body)
        .send()
        .await
        .unwrap();
    (res.status().as_u16(), res.json().await.unwrap())
}

#[tokio::test]
async fn stat_file_over_connect_json() {
    let (addr, _) = spawn_server_with_state().await;
    let tmp = tempfile::tempdir().unwrap();
    let file = std::fs::canonicalize(tmp.path()).unwrap().join("file.txt");
    std::fs::write(&file, b"Hello").unwrap();

    let (status, body) = call(addr, "Stat", &current_user(), json!({ "path": file })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["entry"]["name"], "file.txt");
    assert_eq!(body["entry"]["type"], "FILE_TYPE_FILE");
    assert_eq!(body["entry"]["path"], file.to_str().unwrap());
    assert_eq!(body["entry"]["owner"], current_user());
}

#[tokio::test]
async fn stat_missing_is_not_found() {
    let (addr, _) = spawn_server_with_state().await;
    let (status, body) = call(addr, "Stat", &current_user(), json!({ "path": "/definitely/not/here" })).await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "not_found");
    assert_eq!(
        body["message"],
        "file not found: lstat /definitely/not/here: no such file or directory"
    );
}

#[tokio::test]
async fn basic_user_reaches_the_service() {
    let (addr, _) = spawn_server_with_state().await;
    let me = nix::unistd::User::from_uid(nix::unistd::getuid()).unwrap().unwrap();
    let (status, body) = call(addr, "Stat", &me.name, json!({ "path": "" })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["entry"]["path"], me.dir.to_str().unwrap());
}

#[tokio::test]
async fn unknown_basic_user_is_rejected_on_every_route() {
    let (addr, _) = spawn_server_with_state().await;
    let (status, body) = call(addr, "Stat", "definitely-missing-user", json!({ "path": "/" })).await;
    assert_eq!(status, 401);
    assert_eq!(body["code"], "unauthenticated");
    assert_eq!(body["message"], "invalid username: 'definitely-missing-user'");

    let res = reqwest::Client::new()
        .get(format!("http://{addr}/health"))
        .basic_auth("definitely-missing-user", Some(""))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(res.headers()["content-type"], "application/json");
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": "unauthenticated", "message": "invalid username: 'definitely-missing-user'" })
    );
}

#[tokio::test]
async fn not_yet_ported_methods_are_unimplemented() {
    let (addr, _) = spawn_server_with_state().await;
    let (status, body) = call(addr, "CreateWatcher", &current_user(), json!({ "path": "/" })).await;
    assert_eq!(status, 501);
    assert_eq!(
        body["message"],
        "filesystem.Filesystem.CreateWatcher is not implemented"
    );
}

#[tokio::test]
async fn make_dir_list_move_remove_roundtrip() {
    let (addr, _) = spawn_server_with_state().await;
    let me = current_user();
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let dir = root.join("a/b");

    let (status, body) = call(addr, "MakeDir", &me, json!({ "path": dir })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["entry"]["type"], "FILE_TYPE_DIRECTORY");

    let (status, body) = call(addr, "MakeDir", &me, json!({ "path": dir })).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "already_exists");

    let (status, body) = call(addr, "ListDir", &me, json!({ "path": root, "depth": 2 })).await;
    assert_eq!(status, 200, "{body}");
    let listed: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec![root.join("a").to_str().unwrap(), dir.to_str().unwrap()]);

    let moved = root.join("c");
    let (status, body) = call(
        addr,
        "Move",
        &me,
        json!({ "source": root.join("a"), "destination": moved }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["entry"]["path"], moved.to_str().unwrap());

    let (status, body) = call(addr, "Remove", &me, json!({ "path": moved })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({}));
    assert!(!moved.exists());
}
