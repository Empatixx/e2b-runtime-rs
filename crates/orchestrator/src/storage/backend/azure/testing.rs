use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

const HEADER_END: &str = "\r\n\r\n";

pub async fn read_request(connection: &mut TcpStream) -> String {
    let mut received = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = connection.read(&mut chunk).await.unwrap();
        received.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&received).into_owned();
        if read == 0 || complete(&text) {
            return text;
        }
    }
}

fn complete(text: &str) -> bool {
    text.split_once(HEADER_END).is_some_and(|(head, body)| {
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        body.len() >= length
    })
}

pub async fn serve(replies: Vec<(u16, &'static str, String)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = base.clone();
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for (status, content_type, body) in replies {
            let (mut connection, _) = listener.accept().await.unwrap();
            seen.push(read_request(&mut connection).await);
            let body = body.replace("{BASE}", &served);
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
        }
        seen
    });
    (base, handle)
}
