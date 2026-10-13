pub fn parse_bool(text: &str) -> Result<bool, String> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        other => Err(format!("invalid boolean {other:?}")),
    }
}

pub fn env_bool(value: Option<&str>, field: &str) -> Result<bool, String> {
    value.filter(|value| !value.is_empty()).map_or(Ok(false), |value| {
        parse_bool(value).map_err(|e| format!("{field}: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bools_parse_like_strconv() {
        assert_eq!(parse_bool("T"), Ok(true));
        assert_eq!(parse_bool("False"), Ok(false));
        assert!(parse_bool("yes").is_err());
    }

    #[test]
    fn env_bools_name_the_field() {
        assert_eq!(env_bool(None, "S3_PATH_STYLE"), Ok(false));
        assert_eq!(env_bool(Some(""), "S3_PATH_STYLE"), Ok(false));
        assert_eq!(env_bool(Some("1"), "S3_PATH_STYLE"), Ok(true));
        assert_eq!(
            env_bool(Some("x"), "S3_PATH_STYLE"),
            Err("S3_PATH_STYLE: invalid boolean \"x\"".to_owned())
        );
    }
}
