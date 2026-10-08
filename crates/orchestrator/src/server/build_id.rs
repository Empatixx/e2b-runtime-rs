use uuid::Uuid;

const URN_PREFIX: &str = "urn:uuid:";

pub fn parse(text: &str) -> Result<Uuid, String> {
    let invalid = || "invalid UUID format".to_owned();
    let hex = match text.len() {
        36 => dashed(text)?,
        45 => {
            let prefix = text.get(..URN_PREFIX.len()).ok_or_else(invalid)?;
            if !prefix.eq_ignore_ascii_case(URN_PREFIX) {
                return Err(format!("invalid urn prefix: {prefix:?}"));
            }
            dashed(text.get(URN_PREFIX.len()..).ok_or_else(invalid)?)?
        }
        38 => dashed(text.get(1..37).ok_or_else(invalid)?)?,
        32 => text.to_owned(),
        length => return Err(format!("invalid UUID length: {length}")),
    };
    Uuid::try_parse(&hex)
        .ok()
        .filter(|_| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "invalid UUID format".to_owned())
}

fn dashed(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    [8, 13, 18, 23]
        .iter()
        .all(|index| bytes.get(*index) == Some(&b'-'))
        .then(|| text.replace('-', ""))
        .filter(|hex| hex.len() == 32)
        .ok_or_else(|| "invalid UUID format".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "c9bf9e57-1685-4c89-bafb-ff5af830be8a";

    #[test]
    fn accepts_the_forms_google_uuid_accepts() {
        let id = Uuid::parse_str(ID).unwrap();
        assert_eq!(parse(ID), Ok(id));
        assert_eq!(parse(&format!("urn:uuid:{ID}")), Ok(id));
        assert_eq!(parse(&format!("URN:UUID:{ID}")), Ok(id));
        assert_eq!(parse(&format!("{{{ID}}}")), Ok(id));
        assert_eq!(parse(&ID.replace('-', "")), Ok(id));
    }

    #[test]
    fn reports_google_uuid_errors() {
        assert_eq!(parse("abc"), Err("invalid UUID length: 3".to_owned()));
        assert_eq!(parse(""), Err("invalid UUID length: 0".to_owned()));
        assert_eq!(
            parse(&format!("urx:uuid:{ID}")),
            Err("invalid urn prefix: \"urx:uuid:\"".to_owned())
        );
        assert_eq!(parse(&ID.replace('-', "x")), Err("invalid UUID format".to_owned()));
        assert_eq!(parse(&ID.replace('c', "g")), Err("invalid UUID format".to_owned()));
        assert_eq!(parse(&format!("é{}", &ID[..34])), Err("invalid UUID format".to_owned()));
    }
}
