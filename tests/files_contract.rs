mod common;

use std::path::Path;

use async_compression::tokio::bufread::{GzipDecoder, GzipEncoder};
use common::spawn_server_with_state;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

fn current_user() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap()
        .name
}

fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    (tmp, root)
}

fn files_url(addr: std::net::SocketAddr, path: &Path) -> String {
    format!(
        "http://{addr}/files?path={}&username={}",
        path.display(),
        current_user()
    )
}

async fn upload(addr: std::net::SocketAddr, path: &Path, content: &'static str) -> reqwest::Response {
    let form = reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::text(content).file_name("x"));
    reqwest::Client::new()
        .post(files_url(addr, path))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

async fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzipEncoder::new(bytes).read_to_end(&mut out).await.unwrap();
    out
}

async fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzipDecoder::new(bytes).read_to_end(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn upload_creates_parents_overwrites_and_downloads() {
    let (addr, _) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();
    let target = root.join("a/b/c.txt");

    for content in ["one", "two"] {
        let res = upload(addr, &target, content).await;
        assert_eq!(res.status(), 200);
        let entries: Value = res.json().await.unwrap();
        assert_eq!(
            entries,
            json!([{ "name": "c.txt", "path": target.to_str().unwrap(), "type": "file" }])
        );
    }

    let res = reqwest::get(files_url(addr, &target)).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-disposition"], "inline; filename=c.txt");
    assert_eq!(res.headers()["vary"], "Accept-Encoding");
    assert_eq!(res.text().await.unwrap(), "two");
}

#[tokio::test]
async fn multipart_without_path_uses_file_names() {
    let (addr, _) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();
    let first = root.join("one.txt");
    let second = root.join("dir/two.txt");

    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::text("1").file_name(first.to_str().unwrap().to_owned()),
        )
        .part("ignored", reqwest::multipart::Part::text("x"))
        .part(
            "file",
            reqwest::multipart::Part::text("2").file_name(second.to_str().unwrap().to_owned()),
        );
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/files?username={}", current_user()))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let entries: Value = res.json().await.unwrap();
    assert_eq!(entries[0]["path"], first.to_str().unwrap());
    assert_eq!(entries[1]["path"], second.to_str().unwrap());
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "2");
}

#[tokio::test]
async fn raw_and_gzip_uploads() {
    let (addr, _) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();
    let target = root.join("raw.bin");
    let client = reqwest::Client::new();

    let res = client
        .post(files_url(addr, &target))
        .header("content-type", "application/octet-stream")
        .header("content-encoding", "gzip")
        .body(gzip(b"compressed body").await)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "compressed body");

    let res = client
        .post(format!("http://{addr}/files?username={}", current_user()))
        .header("content-type", "application/octet-stream")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": 400, "message": "path query parameter is required for raw body upload" })
    );

    let res = client
        .post(files_url(addr, &target))
        .header("content-type", "text/plain")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body["message"],
        "unsupported content type: text/plain, expected multipart/form-data or application/octet-stream"
    );
}

#[tokio::test]
async fn download_gzip_when_accepted() {
    let (addr, _) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();
    let target = root.join("data.json");
    std::fs::write(&target, b"{\"a\":1}").unwrap();

    let res = reqwest::Client::new()
        .get(files_url(addr, &target))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-encoding"], "gzip");
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(gunzip(&res.bytes().await.unwrap()).await, b"{\"a\":1}");

    let res = reqwest::Client::new()
        .get(files_url(addr, &target))
        .header("accept-encoding", "br, identity;q=0")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 406);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body["message"],
        "error parsing Accept-Encoding: no acceptable encoding found, supported: [gzip]"
    );
}

#[tokio::test]
async fn download_errors_are_json() {
    let (addr, _) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();

    let res = reqwest::get(files_url(addr, &root.join("missing"))).await.unwrap();
    assert_eq!(res.status(), 404);
    assert_eq!(res.headers()["content-type"], "application/json; charset=utf-8");
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": 404, "message": format!("path '{}' does not exist", root.join("missing").display()) })
    );

    let res = reqwest::get(files_url(addr, &root)).await.unwrap();
    assert_eq!(res.status(), 400);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["message"], format!("path '{}' is a directory", root.display()));

    let res = reqwest::get(format!("http://{addr}/files?path=/x&username=definitely-missing-user"))
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body["message"],
        "error looking up user 'definitely-missing-user': user: unknown user definitely-missing-user"
    );

    let res = reqwest::get(format!("http://{addr}/files?path=/x&signature_expiration=soon"))
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(
        res.text().await.unwrap(),
        "Invalid format for parameter signature_expiration: error binding string parameter: strconv.ParseInt: parsing \"soon\": invalid syntax\n"
    );
}

#[tokio::test]
async fn download_requires_signature_when_token_set() {
    let (addr, state) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();
    let target = root.join("secret.txt");
    std::fs::write(&target, b"s").unwrap();
    *state.access_token.write() = Some("t".into());
    let me = current_user();
    let path = target.to_str().unwrap();
    let url = format!("http://{addr}/files?path={path}&username={me}");

    let res = reqwest::get(&url).await.unwrap();
    assert_eq!(res.status(), 401);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": 401, "message": "missing signature query parameter" })
    );

    let signature = envd::auth::generate_signature(path, &me, envd::auth::READ, "t", None);
    let res = reqwest::get(format!(
        "{url}&signature={}",
        signature.replace('+', "%2B").replace('/', "%2F")
    ))
    .await
    .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.text().await.unwrap(), "s");
}

#[tokio::test]
async fn compose_joins_sources_and_removes_them() {
    let (addr, _) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();
    std::fs::write(root.join("p1"), b"hello ").unwrap();
    std::fs::write(root.join("p2"), b"world").unwrap();
    let destination = root.join("out/joined.txt");

    let res = reqwest::Client::new()
        .post(format!("http://{addr}/files/compose"))
        .json(&json!({
            "source_paths": [root.join("p1"), root.join("p2")],
            "destination": destination,
            "username": current_user(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "name": "joined.txt", "path": destination.to_str().unwrap(), "type": "file" })
    );
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "hello world");
    assert!(!root.join("p1").exists() && !root.join("p2").exists());

    let res = reqwest::Client::new()
        .post(format!("http://{addr}/files/compose"))
        .json(&json!({ "source_paths": ["missing"], "destination": destination, "username": current_user() }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": 404, "message": "source file not found: missing" })
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn upload_stores_metadata_headers_as_xattrs() {
    let (addr, _) = spawn_server_with_state().await;
    let (_tmp, root) = temp_root();
    let target = root.join("meta.txt");

    let res = reqwest::Client::new()
        .post(files_url(addr, &target))
        .header("content-type", "application/octet-stream")
        .header("x-metadata-Owner", "team-a")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let entries: Value = res.json().await.unwrap();
    assert_eq!(entries[0]["metadata"], json!({ "owner": "team-a" }));
}
