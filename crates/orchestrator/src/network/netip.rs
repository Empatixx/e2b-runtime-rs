use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    pub address: IpAddr,
    pub bits: u8,
}

pub fn parse_addr(text: &str) -> Result<IpAddr, String> {
    let failed = |message: &str, at: Option<&str>| match at {
        Some(at) => format!("ParseAddr({text:?}): {message} (at {at:?})"),
        None => format!("ParseAddr({text:?}): {message}"),
    };
    match text.find(['.', ':', '%']).map(|index| text.as_bytes()[index]) {
        Some(b'.') => parse_ipv4(text)
            .map(IpAddr::V4)
            .map_err(|(message, at)| failed(message, at)),
        Some(b':') => parse_ipv6(text)
            .map(IpAddr::V6)
            .map_err(|message| failed(message, None)),
        Some(_) => Err(failed("missing IPv6 address", None)),
        None => Err(failed("unable to parse IP", None)),
    }
}

pub fn parse_prefix(text: &str) -> Result<Prefix, String> {
    let failed = |message: &str| format!("netip.ParsePrefix({text:?}): {message}");
    let (address, bits) = text.rsplit_once('/').ok_or_else(|| failed("no '/'"))?;
    if address.contains('%') && address.contains(':') && parse_addr(address).is_ok() {
        return Err(failed("IPv6 zones cannot be present in a prefix"));
    }
    let address = parse_addr(address).map_err(|e| failed(&e))?;
    let bad_bits = || failed(&format!("bad bits after slash: {bits:?}"));
    if bits.len() > 1 && !bits.starts_with(|first: char| ('1'..='9').contains(&first)) {
        return Err(bad_bits());
    }
    let bits: i64 = bits.parse().map_err(|_| bad_bits())?;
    let max = match address {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    };
    u8::try_from(bits)
        .ok()
        .filter(|bits| *bits <= max)
        .map(|bits| Prefix { address, bits })
        .ok_or_else(|| failed("prefix length out of range"))
}

fn parse_ipv4(text: &str) -> Result<Ipv4Addr, (&'static str, Option<&str>)> {
    let mut fields = [0u8; 4];
    let (mut value, mut position, mut digits) = (0u32, 0usize, 0usize);
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'0'..=b'9' => {
                if digits == 1 && value == 0 {
                    return Err(("IPv4 field has octet with leading zero", None));
                }
                value = value * 10 + u32::from(byte - b'0');
                digits += 1;
                if value > 255 {
                    return Err(("IPv4 field has value >255", None));
                }
            }
            b'.' => {
                if index == 0 || index == bytes.len() - 1 || bytes[index - 1] == b'.' {
                    return Err(("IPv4 field must have at least one digit", Some(&text[index..])));
                }
                if position == 3 {
                    return Err(("IPv4 address too long", None));
                }
                fields[position] = value as u8;
                position += 1;
                value = 0;
                digits = 0;
            }
            _ => return Err(("unexpected character", text.get(index..))),
        }
    }
    if position < 3 {
        return Err(("IPv4 address too short", None));
    }
    fields[3] = value as u8;
    Ok(Ipv4Addr::from(fields))
}

fn parse_ipv6(text: &str) -> Result<Ipv6Addr, &'static str> {
    let (address, zone) = text
        .split_once('%')
        .map_or((text, None), |(address, zone)| (address, Some(zone)));
    if zone == Some("") {
        return Err("zone must be a non-empty string");
    }
    address.parse().map_err(|_| "unable to parse IP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_go_netip() {
        assert_eq!(parse_addr("1.2.3.4"), Ok(IpAddr::from([1, 2, 3, 4])));
        assert_eq!(parse_addr("::1"), Ok(IpAddr::from(Ipv6Addr::LOCALHOST)));
        assert_eq!(
            parse_prefix("10.0.0.1/8"),
            Ok(Prefix {
                address: IpAddr::from([10, 0, 0, 1]),
                bits: 8
            })
        );
    }

    #[test]
    fn reports_go_netip_errors() {
        assert_eq!(
            parse_addr("nope"),
            Err("ParseAddr(\"nope\"): unable to parse IP".to_owned())
        );
        assert_eq!(
            parse_addr("1.2.3"),
            Err("ParseAddr(\"1.2.3\"): IPv4 address too short".to_owned())
        );
        assert_eq!(
            parse_addr("1.2.3.4.5"),
            Err("ParseAddr(\"1.2.3.4.5\"): IPv4 address too long".to_owned())
        );
        assert_eq!(
            parse_addr("1.2.3.256"),
            Err("ParseAddr(\"1.2.3.256\"): IPv4 field has value >255".to_owned())
        );
        assert_eq!(
            parse_addr("1.02.3.4"),
            Err("ParseAddr(\"1.02.3.4\"): IPv4 field has octet with leading zero".to_owned())
        );
        assert_eq!(
            parse_addr("1..3.4"),
            Err("ParseAddr(\"1..3.4\"): IPv4 field must have at least one digit (at \".3.4\")".to_owned())
        );
        assert_eq!(
            parse_addr("1.2.3.x"),
            Err("ParseAddr(\"1.2.3.x\"): unexpected character (at \"x\")".to_owned())
        );
        assert_eq!(
            parse_prefix("nope"),
            Err("netip.ParsePrefix(\"nope\"): no '/'".to_owned())
        );
        assert_eq!(
            parse_prefix("1.2.3/8"),
            Err("netip.ParsePrefix(\"1.2.3/8\"): ParseAddr(\"1.2.3\"): IPv4 address too short".to_owned())
        );
        assert_eq!(
            parse_prefix("1.2.3.0/08"),
            Err("netip.ParsePrefix(\"1.2.3.0/08\"): bad bits after slash: \"08\"".to_owned())
        );
        assert_eq!(
            parse_prefix("1.2.3.0/33"),
            Err("netip.ParsePrefix(\"1.2.3.0/33\"): prefix length out of range".to_owned())
        );
        assert_eq!(
            parse_prefix("fe80::1%eth0/64"),
            Err("netip.ParsePrefix(\"fe80::1%eth0/64\"): IPv6 zones cannot be present in a prefix".to_owned())
        );
    }
}
