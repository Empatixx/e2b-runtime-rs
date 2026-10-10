use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const INITIAL_FLAGS: &str = r#"event: put
data: {"path":"/","data":{"flags":{"local-bool":{"key":"local-bool","on":true,"version":1,"targets":[],"rules":[],"prerequisites":[],"salt":"s","fallthrough":{"variation":0},"variations":[true]},"local-json":{"key":"local-json","on":true,"version":1,"targets":[],"rules":[],"prerequisites":[],"salt":"s","fallthrough":{"variation":0},"variations":[{"enabled":true}]}},"segments":{}}}

"#;

const BOOL_PATCH: &str = r#"event: patch
data: {"path":"/flags/local-bool","data":{"key":"local-bool","on":true,"version":2,"targets":[],"rules":[],"prerequisites":[],"salt":"s","fallthrough":{"variation":0},"variations":[false]}}

"#;

pub struct FakeLaunchDarkly {
    pub url: String,
    pub requests: Arc<AtomicUsize>,
    pub authorizations: Arc<std::sync::Mutex<Vec<String>>>,
    pub event_batches: Arc<Notify>,
    pub patch: Arc<Notify>,
}

impl FakeLaunchDarkly {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Self {
            url: format!("http://{}", listener.local_addr().unwrap()),
            requests: Arc::default(),
            authorizations: Arc::default(),
            event_batches: Arc::default(),
            patch: Arc::default(),
        };
        let shared = fake.shared();
        tokio::spawn(async move {
            loop {
                let (connection, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(connection, shared.clone()));
            }
        });
        fake
    }

    fn shared(&self) -> Shared {
        Shared {
            requests: self.requests.clone(),
            authorizations: self.authorizations.clone(),
            event_batches: self.event_batches.clone(),
            patch: self.patch.clone(),
        }
    }
}

#[derive(Clone)]
struct Shared {
    requests: Arc<AtomicUsize>,
    authorizations: Arc<std::sync::Mutex<Vec<String>>>,
    event_batches: Arc<Notify>,
    patch: Arc<Notify>,
}

async fn serve(mut connection: TcpStream, shared: Shared) {
    let Some((path, headers)) = read_request(&mut connection).await else {
        return;
    };
    shared.requests.fetch_add(1, Ordering::SeqCst);
    headers
        .get("authorization")
        .iter()
        .for_each(|value| shared.authorizations.lock().unwrap().push((*value).clone()));
    match path.as_str() {
        "/all" => stream(&mut connection, &shared).await,
        "/bulk" => accept_events(&mut connection, &shared).await,
        _ => respond(&mut connection, "202 Accepted").await,
    }
}

async fn stream(connection: &mut TcpStream, shared: &Shared) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n";
    connection.write_all(head.as_bytes()).await.ok();
    connection.write_all(INITIAL_FLAGS.as_bytes()).await.ok();
    shared.patch.notified().await;
    connection.write_all(BOOL_PATCH.as_bytes()).await.ok();
    std::future::pending::<()>().await;
}

async fn accept_events(connection: &mut TcpStream, shared: &Shared) {
    respond(connection, "202 Accepted").await;
    shared.event_batches.notify_one();
}

async fn respond(connection: &mut TcpStream, status: &str) {
    let response = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    connection.write_all(response.as_bytes()).await.ok();
    connection.shutdown().await.ok();
}

async fn read_request(connection: &mut TcpStream) -> Option<(String, HashMap<String, String>)> {
    let mut received = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = connection.read(&mut chunk).await.ok().filter(|read| *read > 0)?;
        received.extend_from_slice(&chunk[..read]);
        if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    let mut lines = head.lines();
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    let body_length = headers
        .get("content-length")
        .and_then(|length| length.parse().ok())
        .unwrap_or(0);
    let already = received.len() - head_end;
    if already < body_length {
        let mut rest = vec![0u8; body_length - already];
        connection.read_exact(&mut rest).await.ok()?;
    }
    Some((path, headers))
}
