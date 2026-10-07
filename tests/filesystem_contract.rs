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
async fn unknown_watcher_is_not_found() {
    let (addr, _) = spawn_server_with_state().await;
    for method in ["GetWatcherEvents", "RemoveWatcher"] {
        let (status, body) = call(addr, method, &current_user(), json!({ "watcherId": "wmissing" })).await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["message"], "watcher with id wmissing not found");
    }
}

#[tokio::test]
async fn create_watcher_rejects_missing_path_and_files() {
    let (addr, _) = spawn_server_with_state().await;
    let (status, body) = call(
        addr,
        "CreateWatcher",
        &current_user(),
        json!({ "path": "/definitely/not/here" }),
    )
    .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(
        body["message"],
        "path /definitely/not/here not found: stat /definitely/not/here: no such file or directory"
    );

    let tmp = tempfile::tempdir().unwrap();
    let file = std::fs::canonicalize(tmp.path()).unwrap().join("file.txt");
    std::fs::write(&file, b"x").unwrap();
    let (status, body) = call(addr, "CreateWatcher", &current_user(), json!({ "path": file })).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["message"],
        format!("path {} not a directory: %!w(<nil>)", file.display())
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn watcher_collects_events_until_removed() {
    let (addr, _) = spawn_server_with_state().await;
    let me = current_user();
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();

    let (status, body) = call(
        addr,
        "CreateWatcher",
        &me,
        json!({ "path": root, "includeEntry": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let id = body["watcherId"].as_str().unwrap().to_owned();
    assert!(id.starts_with('w') && id.len() == 21, "{id}");

    std::fs::write(root.join("a.txt"), b"hello").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (status, body) = call(addr, "GetWatcherEvents", &me, json!({ "watcherId": id })).await;
    assert_eq!(status, 200, "{body}");
    let first = &body["events"][0];
    assert_eq!(first["name"], "a.txt");
    assert_eq!(first["type"], "EVENT_TYPE_CREATE");
    assert_eq!(first["entry"]["name"], "a.txt");

    let (_, body) = call(addr, "GetWatcherEvents", &me, json!({ "watcherId": id })).await;
    assert_eq!(body, json!({}));

    let (status, _) = call(addr, "RemoveWatcher", &me, json!({ "watcherId": id })).await;
    assert_eq!(status, 200);
    let (status, _) = call(addr, "GetWatcherEvents", &me, json!({ "watcherId": id })).await;
    assert_eq!(status, 404);
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

async fn next_message(res: &mut reqwest::Response, buffer: &mut Vec<u8>) -> Value {
    loop {
        if buffer.len() >= 5 {
            let len = u32::from_be_bytes(buffer[1..5].try_into().unwrap()) as usize;
            if buffer.len() >= 5 + len {
                let message: Vec<u8> = buffer.drain(..5 + len).skip(5).collect();
                return serde_json::from_slice(&message).unwrap();
            }
        }
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), res.chunk())
            .await
            .expect("stream message in time")
            .unwrap()
            .expect("stream still open");
        buffer.extend_from_slice(&chunk);
    }
}

fn envelope(body: Value) -> Vec<u8> {
    let json = serde_json::to_vec(&body).unwrap();
    let mut framed = vec![0];
    framed.extend_from_slice(&(json.len() as u32).to_be_bytes());
    framed.extend_from_slice(&json);
    framed
}

#[tokio::test]
async fn watch_dir_streams_start_events_and_keepalives() {
    let (addr, _) = spawn_server_with_state().await;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();

    let mut res = reqwest::Client::new()
        .post(format!("http://{addr}/filesystem.Filesystem/WatchDir"))
        .header("content-type", "application/connect+json")
        .header("keepalive-ping-interval", "1")
        .basic_auth(current_user(), Some(""))
        .body(envelope(json!({ "path": root })))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let mut buffer = Vec::new();
    assert_eq!(next_message(&mut res, &mut buffer).await, json!({ "start": {} }));

    #[cfg(target_os = "linux")]
    {
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        let event = next_message(&mut res, &mut buffer).await;
        assert_eq!(event["filesystem"]["name"], "a.txt", "{event}");
        assert_eq!(event["filesystem"]["type"], "EVENT_TYPE_CREATE", "{event}");
    }

    let keepalive = loop {
        let message = next_message(&mut res, &mut buffer).await;
        if message.get("keepalive").is_some() {
            break message;
        }
    };
    assert_eq!(keepalive, json!({ "keepalive": {} }));
}
