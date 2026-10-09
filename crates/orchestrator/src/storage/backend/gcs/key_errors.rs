use ring::signature::RsaKeyPair;

const PARAMS: &str = "{optional:false explicit:false application:false private:false defaultValue:<nil> tag:<nil> stringType:0 timeType:0 set:false omitEmpty:false}";
const SEQUENCE: u8 = 0x30;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Pkcs8,
    Pkcs1,
    Ec,
    Unknown,
}

struct Header {
    class: u8,
    tag: u32,
    compound: bool,
    length: usize,
    offset: usize,
}

fn header(der: &[u8]) -> Result<Header, String> {
    let syntax = |text: &str| format!("asn1: syntax error: {text}");
    let first = *der.first().ok_or_else(|| syntax("sequence truncated"))?;
    let class = first >> 6;
    let compound = first & 0x20 != 0;
    let mut offset = 1;
    let mut tag = u32::from(first & 0x1f);
    if tag == 0x1f {
        tag = 0;
        loop {
            let byte = *der.get(offset).ok_or_else(|| syntax("truncated tag or length"))?;
            offset += 1;
            tag = tag << 7 | u32::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
    }
    let length_byte = *der.get(offset).ok_or_else(|| syntax("truncated tag or length"))?;
    offset += 1;
    let length = match length_byte & 0x80 {
        0 => usize::from(length_byte),
        _ => long_length(der, &mut offset, usize::from(length_byte & 0x7f))?,
    };
    Ok(Header {
        class,
        tag,
        compound,
        length,
        offset,
    })
}

fn long_length(der: &[u8], offset: &mut usize, count: usize) -> Result<usize, String> {
    let syntax = |text: &str| format!("asn1: syntax error: {text}");
    match count {
        0 => return Err(syntax("indefinite length found (not DER)")),
        1..=4 => {}
        _ => return Err(syntax("length too large")),
    }
    let mut length = 0usize;
    for _ in 0..count {
        let byte = *der.get(*offset).ok_or_else(|| syntax("truncated tag or length"))?;
        *offset += 1;
        length = length << 8 | usize::from(byte);
    }
    Ok(length)
}

fn container(der: &[u8], type_name: &str) -> Result<Header, String> {
    let parsed = header(der)?;
    if parsed.class != 0 || parsed.tag != 16 || !parsed.compound {
        return Err(format!(
            "asn1: structure error: tags don't match (16 vs {{class:{} tag:{} length:{} isCompound:{}}}) {PARAMS} {type_name} @{}",
            parsed.class, parsed.tag, parsed.length, parsed.compound, parsed.offset
        ));
    }
    match parsed.offset.checked_add(parsed.length) {
        Some(end) if end <= der.len() => Ok(parsed),
        _ => Err("asn1: syntax error: data truncated".to_owned()),
    }
}

fn shape(der: &[u8]) -> Shape {
    let Ok(outer) = header(der) else {
        return Shape::Unknown;
    };
    if outer.class != 0 || outer.tag != 16 || der.first() != Some(&SEQUENCE) {
        return Shape::Unknown;
    }
    let body = &der[outer.offset.min(der.len())..];
    let (Ok(version), true) = (header(body), body.first() == Some(&0x02)) else {
        return Shape::Unknown;
    };
    let after = body.get(version.offset + version.length..).unwrap_or_default();
    let value = body.get(version.offset).copied().unwrap_or(0xff);
    match (value, after.first()) {
        (0, Some(&SEQUENCE)) => Shape::Pkcs8,
        (0, Some(&0x02)) => Shape::Pkcs1,
        (1, Some(&0x04)) => Shape::Ec,
        _ => Shape::Unknown,
    }
}

fn hint(other: &str) -> String {
    format!("x509: failed to parse private key (use {other} instead for this key format)")
}

pub fn pkcs8_error(der: &[u8]) -> String {
    match shape(der) {
        Shape::Pkcs1 => hint("ParsePKCS1PrivateKey"),
        Shape::Ec => hint("ParseECPrivateKey"),
        _ => container(der, "pkcs8")
            .err()
            .unwrap_or_else(|| "asn1: structure error: unsupported PKCS8 private key".to_owned()),
    }
}

pub fn pkcs1_error(der: &[u8]) -> String {
    match shape(der) {
        Shape::Pkcs8 => hint("ParsePKCS8PrivateKey"),
        Shape::Ec => hint("ParseECPrivateKey"),
        _ => container(der, "pkcs1PrivateKey")
            .err()
            .unwrap_or_else(|| "asn1: structure error: unsupported PKCS1 private key".to_owned()),
    }
}

pub fn ec_error(der: &[u8]) -> String {
    match shape(der) {
        Shape::Pkcs8 => hint("ParsePKCS8PrivateKey"),
        Shape::Pkcs1 => hint("ParsePKCS1PrivateKey"),
        _ => format!(
            "x509: failed to parse EC private key: {}",
            container(der, "ecPrivateKey")
                .err()
                .unwrap_or_else(|| "asn1: structure error: unsupported EC private key".to_owned())
        ),
    }
}

pub fn parse_key_failure(der: &[u8]) -> String {
    format!(
        "failed to parse private key. Tried PKCS8, PKCS1, and EC formats. Errors: [PKCS8: {}], [PKCS1: {}], [EC: {}]",
        pkcs8_error(der),
        pkcs1_error(der),
        ec_error(der)
    )
}

pub fn rsa_key(der: &[u8]) -> Result<RsaKeyPair, String> {
    RsaKeyPair::from_pkcs8(der)
        .or_else(|_| RsaKeyPair::from_der(der))
        .map_err(|_| match shape(der) {
            Shape::Pkcs8 => "oauth2: private key is invalid".to_owned(),
            _ => pkcs1_error(der),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_reports_the_tag_mismatch_like_go() {
        let expected = |name: &str| {
            format!(
                "asn1: structure error: tags don't match (16 vs {{class:1 tag:14 length:111 isCompound:true}}) {PARAMS} {name} @2"
            )
        };

        assert_eq!(pkcs8_error(b"nope"), expected("pkcs8"));
        assert_eq!(pkcs1_error(b"nope"), expected("pkcs1PrivateKey"));
        assert_eq!(
            ec_error(b"nope"),
            format!("x509: failed to parse EC private key: {}", expected("ecPrivateKey"))
        );
    }

    #[test]
    fn truncations_have_their_own_texts() {
        assert_eq!(pkcs8_error(b""), "asn1: syntax error: sequence truncated");
        assert_eq!(pkcs8_error(&[0x30]), "asn1: syntax error: truncated tag or length");
        assert_eq!(
            pkcs1_error(&[0x30, 0x03, 0x01, 0x02]),
            "asn1: syntax error: data truncated"
        );
    }

    #[test]
    fn wrong_formats_name_the_right_parser() {
        let rsa = crate::storage::backend::pem::blocks(include_str!("test_key.pem"))
            .remove(0)
            .der;
        let ec = crate::storage::backend::pem::blocks(include_str!("test_ec_sec1.pem"))
            .remove(0)
            .der;

        assert_eq!(
            pkcs1_error(&rsa),
            "x509: failed to parse private key (use ParsePKCS8PrivateKey instead for this key format)"
        );
        assert_eq!(
            pkcs8_error(&ec),
            "x509: failed to parse private key (use ParseECPrivateKey instead for this key format)"
        );
        assert_eq!(
            ec_error(&rsa),
            "x509: failed to parse private key (use ParsePKCS8PrivateKey instead for this key format)"
        );
    }
}
