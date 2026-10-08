pub fn parse_bool(text: &str) -> Result<bool, String> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        other => Err(format!("strconv.ParseBool: parsing {}: invalid syntax", quote(other))),
    }
}

pub fn env_bool(value: Option<&str>, field: &str) -> Result<bool, String> {
    value.filter(|value| !value.is_empty()).map_or(Ok(false), |value| {
        parse_bool(value).map_err(|e| format!("env: parse error on field {} of type \"bool\": {e}", quote(field)))
    })
}

pub fn quote(text: &str) -> String {
    let body: String = text.chars().map(quoted_char).collect();
    format!("\"{body}\"")
}

fn quoted_char(c: char) -> String {
    match c {
        '"' => "\\\"".to_owned(),
        '\\' => "\\\\".to_owned(),
        '\u{7}' => "\\a".to_owned(),
        '\u{8}' => "\\b".to_owned(),
        '\u{c}' => "\\f".to_owned(),
        '\n' => "\\n".to_owned(),
        '\r' => "\\r".to_owned(),
        '\t' => "\\t".to_owned(),
        '\u{b}' => "\\v".to_owned(),
        c if (c as u32) < 0x20 || c == '\u{7f}' => format!("\\x{:02x}", c as u32),
        c if c.is_control() && (c as u32) <= 0xffff => format!("\\u{:04x}", c as u32),
        c if c.is_control() => format!("\\U{:08x}", c as u32),
        c => c.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bools_parse_like_strconv() {
        assert_eq!(parse_bool("T"), Ok(true));
        assert_eq!(parse_bool("False"), Ok(false));
        assert_eq!(
            parse_bool("yes"),
            Err("strconv.ParseBool: parsing \"yes\": invalid syntax".to_owned())
        );
    }

    #[test]
    fn env_bools_name_the_field() {
        assert_eq!(env_bool(None, "ForceStop"), Ok(false));
        assert_eq!(env_bool(Some(""), "ForceStop"), Ok(false));
        assert_eq!(env_bool(Some("1"), "ForceStop"), Ok(true));
        assert_eq!(
            env_bool(Some("x"), "S3PathStyle"),
            Err(
                "env: parse error on field \"S3PathStyle\" of type \"bool\": strconv.ParseBool: parsing \"x\": invalid syntax"
                    .to_owned()
            )
        );
    }

    #[test]
    fn quotes_like_go() {
        assert_eq!(quote("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\x01\"");
        assert_eq!(quote("žluť"), "\"žluť\"");
    }
}
