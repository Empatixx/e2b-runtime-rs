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
