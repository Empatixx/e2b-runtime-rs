use crate::go_strconv::quote;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Path,
    Host,
    Zone,
    UserPassword,
    QueryComponent,
    Fragment,
}

const HOST_SUB_DELIMS: &[u8] = b"!$&'()*+,;=:[]<>\"";
const ENCODED_EXTRA: &[u8] = b"!$&'()*+,;=:@[]%";

pub fn should_escape(c: u8, mode: Mode) -> bool {
    match c {
        c if c.is_ascii_alphanumeric() => false,
        c if matches!(mode, Mode::Host | Mode::Zone) && HOST_SUB_DELIMS.contains(&c) => false,
        b'-' | b'_' | b'.' | b'~' => false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => reserved_escapes(c, mode),
        b'!' | b'(' | b')' | b'*' => mode != Mode::Fragment,
        _ => true,
    }
}

fn reserved_escapes(c: u8, mode: Mode) -> bool {
    match mode {
        Mode::Path => c == b'?',
        Mode::UserPassword => matches!(c, b'@' | b'/' | b'?' | b':'),
        Mode::QueryComponent | Mode::Host | Mode::Zone => true,
        Mode::Fragment => false,
    }
}

pub fn escape(text: &str, mode: Mode) -> String {
    text.bytes()
        .map(|c| match c {
            b' ' if mode == Mode::QueryComponent => "+".to_owned(),
            c if should_escape(c, mode) => format!("%{c:02X}"),
            c => (c as char).to_string(),
        })
        .collect()
}

pub fn unescape(text: &str, mode: Mode) -> Result<String, String> {
    validate_escapes(text.as_bytes(), mode)?;
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                decoded.push(hex_value(bytes[index + 1]) << 4 | hex_value(bytes[index + 2]));
                index += 3;
            }
            b'+' if mode == Mode::QueryComponent => {
                decoded.push(b' ');
                index += 1;
            }
            c => {
                decoded.push(c);
                index += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&decoded).into_owned())
}

pub fn valid_encoded(text: &str, mode: Mode) -> bool {
    text.bytes()
        .all(|c| ENCODED_EXTRA.contains(&c) || !should_escape(c, mode))
}

fn validate_escapes(bytes: &[u8], mode: Mode) -> Result<(), String> {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                validate_percent(bytes, index, mode)?;
                index += 3;
            }
            c if matches!(mode, Mode::Host | Mode::Zone) && c < 0x80 && should_escape(c, mode) => {
                return Err(format!(
                    "invalid character {} in host name",
                    quote(&(c as char).to_string())
                ));
            }
            _ => index += 1,
        }
    }
    Ok(())
}

fn validate_percent(bytes: &[u8], index: usize, mode: Mode) -> Result<(), String> {
    let well_formed = index + 2 < bytes.len() && is_hex(bytes[index + 1]) && is_hex(bytes[index + 2]);
    if !well_formed {
        let end = bytes.len().min(index + 3);
        return Err(escape_error(&bytes[index..end]));
    }
    let sequence = &bytes[index..index + 3];
    let value = hex_value(bytes[index + 1]) << 4 | hex_value(bytes[index + 2]);
    let rejected = match mode {
        Mode::Host => hex_value(bytes[index + 1]) < 8 && sequence != b"%25",
        Mode::Zone => sequence != b"%25" && value != b' ' && should_escape(value, Mode::Host),
        _ => false,
    };
    match rejected {
        true => Err(escape_error(sequence)),
        false => Ok(()),
    }
}

fn escape_error(sequence: &[u8]) -> String {
    format!("invalid URL escape {}", quote(&String::from_utf8_lossy(sequence)))
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn hex_value(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_per_mode() {
        assert_eq!(escape("/a b?c%", Mode::Path), "/a%20b%3Fc%25");
        assert_eq!(escape("a b&c=d", Mode::QueryComponent), "a+b%26c%3Dd");
        assert_eq!(escape("bucket", Mode::Host), "bucket");
    }

    #[test]
    fn unescapes_and_names_bad_sequences() {
        assert_eq!(unescape("a%20b+c", Mode::Path), Ok("a b+c".to_owned()));
        assert_eq!(unescape("a+b", Mode::QueryComponent), Ok("a b".to_owned()));
        assert_eq!(
            unescape("a%zzb", Mode::Path),
            Err("invalid URL escape \"%zz\"".to_owned())
        );
        assert_eq!(unescape("a%4", Mode::Path), Err("invalid URL escape \"%4\"".to_owned()));
        assert_eq!(
            unescape("a%2Fb", Mode::Host),
            Err("invalid URL escape \"%2F\"".to_owned())
        );
        assert_eq!(
            unescape("a b", Mode::Host),
            Err("invalid character \" \" in host name".to_owned())
        );
    }
}
