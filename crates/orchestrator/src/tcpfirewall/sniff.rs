use httparse::{EMPTY_HEADER, Request};
use tls_parser::{SNIType, TlsExtension, TlsMessage, TlsMessageHandshake};

const MAX_HEADERS: usize = 128;

pub fn http_host(bytes: &[u8]) -> Option<String> {
    let mut headers = [EMPTY_HEADER; MAX_HEADERS];
    let mut request = Request::new(&mut headers);
    request.parse(bytes).ok()?.is_complete().then_some(())?;
    request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("host"))
        .and_then(|header| std::str::from_utf8(header.value).ok())
        .map(|value| strip_port(value.trim()).to_owned())
        .filter(|host| !host.is_empty())
}

pub fn http_head_complete(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"\r\n\r\n")
}

pub fn tls_record_complete(bytes: &[u8]) -> bool {
    bytes.len() >= 5 && bytes.len() >= 5 + u16::from_be_bytes([bytes[3], bytes[4]]) as usize
}

pub fn tls_sni(bytes: &[u8]) -> Option<String> {
    let (_, record) = tls_parser::parse_tls_plaintext(bytes).ok()?;
    let extensions = record.msg.into_iter().find_map(|message| match message {
        TlsMessage::Handshake(TlsMessageHandshake::ClientHello(hello)) => hello.ext,
        _ => None,
    })?;
    let (_, extensions) = tls_parser::parse_tls_client_hello_extensions(extensions).ok()?;
    extensions
        .into_iter()
        .find_map(|extension| match extension {
            TlsExtension::SNI(names) => names.into_iter().find(|(kind, _)| *kind == SNIType::HostName),
            _ => None,
        })
        .and_then(|(_, name)| String::from_utf8(name.to_vec()).ok())
}

fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(']') && port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_hello(name: &str) -> Vec<u8> {
        let server_name = [
            &[0u8, 0][..],
            &((name.len() + 5) as u16).to_be_bytes(),
            &((name.len() + 3) as u16).to_be_bytes(),
            &[0],
            &(name.len() as u16).to_be_bytes(),
            name.as_bytes(),
        ]
        .concat();
        let hello_body = [
            &[0x03, 0x03][..],
            &[7; 32],
            &[0],
            &[0, 2, 0x13, 0x01],
            &[1, 0],
            &(server_name.len() as u16).to_be_bytes(),
            &server_name,
        ]
        .concat();
        let handshake = [&[1u8][..], &(hello_body.len() as u32).to_be_bytes()[1..], &hello_body].concat();
        [&[0x16u8, 3, 1][..], &(handshake.len() as u16).to_be_bytes(), &handshake].concat()
    }

    #[test]
    fn sni_is_read_from_a_client_hello() {
        let hello = client_hello("api.example.com");

        assert!(tls_record_complete(&hello));
        assert!(!tls_record_complete(&hello[..10]));
        assert_eq!(tls_sni(&hello), Some("api.example.com".to_owned()));
        assert_eq!(tls_sni(b"GET / HTTP/1.1\r\n"), None);
    }

    #[test]
    fn host_header_without_port() {
        let request = b"GET /x HTTP/1.1\r\nhost: example.com:8080\r\nAccept: */*\r\n\r\n";

        assert!(http_head_complete(request));
        assert_eq!(http_host(request), Some("example.com".to_owned()));
        assert_eq!(http_host(b"\x16\x03\x01"), None);
    }
}
