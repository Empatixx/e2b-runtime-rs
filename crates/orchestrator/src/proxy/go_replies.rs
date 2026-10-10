const ERROR_HEADERS: &str = "\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n";
const HYPER_BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n";
const HYPER_HEADERS_TOO_LARGE: &[u8] =
    b"HTTP/1.1 431 Request Header Fields Too Large\r\nconnection: close\r\ncontent-length: 0\r\n";

pub fn go_reply_for(hyper_reply: &[u8]) -> Option<Vec<u8>> {
    [
        (HYPER_BAD_REQUEST, "400 Bad Request"),
        (HYPER_HEADERS_TOO_LARGE, "431 Request Header Fields Too Large"),
    ]
    .iter()
    .find(|(hyper, _)| hyper_reply.starts_with(hyper))
    .map(|(_, status)| format!("HTTP/1.1 {status}{ERROR_HEADERS}{status}").into_bytes())
}

pub fn missing_host_reply() -> String {
    const STATUS: &str = "400 Bad Request: missing required Host header";
    format!("HTTP/1.1 {STATUS}{ERROR_HEADERS}{STATUS}")
}

#[derive(Default)]
pub struct Replacement {
    bytes: Vec<u8>,
    written: usize,
    consumed: usize,
}

impl Replacement {
    pub fn of(hyper_reply: &[u8]) -> Option<Self> {
        go_reply_for(hyper_reply).map(|bytes| Self {
            bytes,
            written: 0,
            consumed: hyper_reply.len(),
        })
    }

    pub fn remaining(&self) -> &[u8] {
        &self.bytes[self.written..]
    }

    pub fn advance(&mut self, written: usize) -> Option<usize> {
        self.written += written;
        (self.written >= self.bytes.len()).then_some(self.consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_hyper_error_replies_with_go_texts() {
        let bad = b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\ndate: x\r\n\r\n";
        assert_eq!(
            String::from_utf8(go_reply_for(bad).unwrap()).unwrap(),
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request"
        );
        let large = b"HTTP/1.1 431 Request Header Fields Too Large\r\nconnection: close\r\ncontent-length: 0\r\n";
        assert!(
            String::from_utf8(go_reply_for(large).unwrap())
                .unwrap()
                .ends_with("\r\n\r\n431 Request Header Fields Too Large")
        );
        assert!(go_reply_for(b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\n").is_none());
        assert!(go_reply_for(b"HTTP/1.1 200 OK\r\n").is_none());
    }

    #[test]
    fn a_replacement_reports_the_consumed_length_once_fully_written() {
        let mut replacement =
            Replacement::of(b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n").unwrap();
        let total = replacement.remaining().len();
        assert_eq!(replacement.advance(5), None);
        assert_eq!(replacement.remaining().len(), total - 5);
        assert!(replacement.advance(total - 5).is_some());
    }
}
