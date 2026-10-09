const LIMIT: u64 = 1 << 63;

struct Cursor<'a> {
    rest: &'a str,
}

impl<'a> Cursor<'a> {
    fn starts_with_digit_or_dot(&self) -> bool {
        self.rest
            .bytes()
            .next()
            .is_some_and(|c| c == b'.' || c.is_ascii_digit())
    }

    fn leading_int(&mut self, original: &str) -> Result<u64, String> {
        let digits = self.rest.bytes().take_while(u8::is_ascii_digit).count();
        let value = self.rest.as_bytes()[..digits].iter().try_fold(0u64, |value, digit| {
            Some(value)
                .filter(|value| *value <= LIMIT / 10)
                .map(|value| value * 10 + u64::from(digit - b'0'))
                .filter(|value| *value <= LIMIT)
        });
        self.rest = &self.rest[digits..];
        value.ok_or_else(|| invalid(original))
    }

    fn leading_fraction(&mut self) -> (u64, f64) {
        let digits = self.rest.bytes().take_while(u8::is_ascii_digit).count();
        let (value, scale, _) =
            self.rest.as_bytes()[..digits]
                .iter()
                .fold((0u64, 1f64, false), |(value, scale, overflowed), digit| {
                    if overflowed || value > (LIMIT - 1) / 10 {
                        return (value, scale, true);
                    }
                    let next = value * 10 + u64::from(digit - b'0');
                    match next > LIMIT {
                        true => (value, scale, true),
                        false => (next, scale * 10.0, false),
                    }
                });
        self.rest = &self.rest[digits..];
        (value, scale)
    }

    fn unit(&mut self, original: &str) -> Result<u64, String> {
        let end = self
            .rest
            .bytes()
            .position(|c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(self.rest.len());
        if end == 0 {
            return Err(format!("time: missing unit in duration {}", quote(original)));
        }
        let name = &self.rest[..end];
        self.rest = &self.rest[end..];
        match name {
            "ns" => Ok(1),
            "us" | "\u{b5}s" | "\u{3bc}s" => Ok(1_000),
            "ms" => Ok(1_000_000),
            "s" => Ok(1_000_000_000),
            "m" => Ok(60_000_000_000),
            "h" => Ok(3_600_000_000_000),
            _ => Err(format!(
                "time: unknown unit {} in duration {}",
                quote(name),
                quote(original)
            )),
        }
    }
}

fn quote(text: &str) -> String {
    crate::go_strconv::quote(text)
}

fn invalid(original: &str) -> String {
    format!("time: invalid duration {}", quote(original))
}

fn component(cursor: &mut Cursor<'_>, original: &str) -> Result<u64, String> {
    if !cursor.starts_with_digit_or_dot() {
        return Err(invalid(original));
    }
    let before = cursor.rest.len();
    let whole = cursor.leading_int(original)?;
    let pre = cursor.rest.len() != before;
    let (fraction, scale, post) = match cursor.rest.strip_prefix('.') {
        Some(after) => {
            cursor.rest = after;
            let before = cursor.rest.len();
            let (fraction, scale) = cursor.leading_fraction();
            (fraction, scale, cursor.rest.len() != before)
        }
        None => (0, 1.0, false),
    };
    if !pre && !post {
        return Err(invalid(original));
    }
    let unit = cursor.unit(original)?;
    let overflow = || format!("time: invalid duration {}", quote(original));
    let mut value = whole
        .checked_mul(unit)
        .filter(|value| *value <= LIMIT)
        .ok_or_else(overflow)?;
    if fraction > 0 {
        value += (fraction as f64 * (unit as f64 / scale)) as u64;
        if value > LIMIT {
            return Err(overflow());
        }
    }
    Ok(value)
}

pub fn parse_duration(text: &str) -> Result<i64, String> {
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if unsigned == "0" {
        return Ok(0);
    }
    if unsigned.is_empty() {
        return Err(invalid(text));
    }
    let mut cursor = Cursor { rest: unsigned };
    let mut total = 0u64;
    while !cursor.rest.is_empty() {
        total += component(&mut cursor, text)?;
        if total > LIMIT {
            return Err(invalid(text));
        }
    }
    match negative {
        true => Ok((total as i128).wrapping_neg() as i64),
        false => i64::try_from(total).map_err(|_| invalid(text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_go() {
        let cases = [
            ("0", 0),
            ("10s", 10_000_000_000),
            ("1h2m3s", 3_723_000_000_000),
            ("1.5s", 1_500_000_000),
            ("-2ms", -2_000_000),
            ("+3us", 3_000),
            ("4\u{b5}s", 4_000),
            (".5s", 500_000_000),
            ("5.s", 5_000_000_000),
        ];
        for (text, nanos) in cases {
            assert_eq!(parse_duration(text), Ok(nanos), "{text}");
        }
    }

    #[test]
    fn rejects_like_go() {
        assert_eq!(parse_duration(""), Err("time: invalid duration \"\"".to_owned()));
        assert_eq!(
            parse_duration("10"),
            Err("time: missing unit in duration \"10\"".to_owned())
        );
        assert_eq!(
            parse_duration("10x"),
            Err("time: unknown unit \"x\" in duration \"10x\"".to_owned())
        );
        assert_eq!(parse_duration("s"), Err("time: invalid duration \"s\"".to_owned()));
        assert_eq!(parse_duration("."), Err("time: invalid duration \".\"".to_owned()));
        assert!(parse_duration("9223372036854775808ns").is_err());
        assert_eq!(parse_duration("-9223372036854775808ns"), Ok(i64::MIN));
    }
}
