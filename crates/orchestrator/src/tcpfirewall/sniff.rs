pub fn http_host(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let head = text.split("\r\n\r\n").next()?;
    let mut lines = head.split("\r\n");
    lines.next().filter(|request| request.split(' ').count() == 3)?;
    lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("host"))
        .map(|(_, value)| strip_port(value.trim()).to_owned())
        .filter(|host| !host.is_empty())
}

pub fn http_head_complete(bytes: &[u8]) -> bool {
    bytes.windows(4).any(|window| window == b"\r\n\r\n")
}

pub fn tls_record_complete(bytes: &[u8]) -> bool {
    bytes.len() >= 5 && bytes.len() >= 5 + u16::from_be_bytes([bytes[3], bytes[4]]) as usize
}

pub fn tls_sni(bytes: &[u8]) -> Option<String> {
    let mut reader = Reader { bytes, position: 0 };
    (reader.u8()? == 0x16).then_some(())?;
    reader.skip(4)?;
    (reader.u8()? == 0x01).then_some(())?;
    reader.skip(3 + 2 + 32)?;
    let session = reader.u8()? as usize;
    reader.skip(session)?;
    let ciphers = reader.u16()? as usize;
    reader.skip(ciphers)?;
    let compression = reader.u8()? as usize;
    reader.skip(compression)?;
    let extensions_end = reader.u16()? as usize + reader.position;
    while reader.position < extensions_end {
        let kind = reader.u16()?;
        let length = reader.u16()? as usize;
        if kind != 0 {
            reader.skip(length)?;
            continue;
        }
        reader.skip(2)?;
        (reader.u8()? == 0).then_some(())?;
        let name = reader.u16()? as usize;
        return reader.take(name).and_then(|name| String::from_utf8(name.to_vec()).ok());
    }
    None
}

fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(']') && port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.position..self.position + length)?;
        self.position += length;
        Some(slice)
    }

    fn skip(&mut self, length: usize) -> Option<()> {
        self.take(length).map(|_| ())
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
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
