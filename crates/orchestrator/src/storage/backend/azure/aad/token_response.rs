use std::time::{Duration, SystemTime};

use chrono::{DateTime, NaiveDateTime};
use serde::Deserialize;
use serde_json::Value;

use super::super::token_source::BearerToken;

const LONG_LIVED: Duration = Duration::from_secs(2 * 60 * 60);
const EXPIRES_ON_FORMATS: [&str; 2] = ["%m/%d/%Y %H:%M:%S", "%Y-%m-%d %H:%M:%S"];

pub struct Grant {
    pub token: BearerToken,
    pub refresh_token: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Raw {
    access_token: String,
    refresh_token: String,
    error: String,
    error_description: String,
    scope: String,
    expires_in: Value,
    expires_on: Value,
    refresh_in: Value,
    ext_expires_in: Value,
}

impl Grant {
    pub fn without_refresh_token(token: BearerToken) -> Self {
        Self {
            token,
            refresh_token: None,
        }
    }
}

pub fn parse(body: &str, requested_scope: &str) -> Result<Grant, String> {
    let decoded = decode(body).map_err(|e| format!("json decode error: {e}\nraw message was: {body}"))?;
    let declined = declined_scopes(requested_scope, &decoded.raw.scope);
    match (
        decoded.raw.error.is_empty(),
        decoded.raw.access_token.is_empty(),
        declined.is_empty(),
    ) {
        (false, _, _) => Err(format!("{}: {}", decoded.raw.error, decoded.raw.error_description)),
        (true, true, _) => Err("response is missing access_token".to_owned()),
        (true, false, false) => Err(format!(
            "token response failed because declined scopes are present: {}",
            declined.join(",")
        )),
        (true, false, true) => Ok(Grant {
            token: BearerToken {
                secret: decoded.raw.access_token,
                expires_at: decoded.expires_at,
                refresh_on: decoded.refresh_on,
            },
            refresh_token: Some(decoded.raw.refresh_token).filter(|token| !token.is_empty()),
        }),
    }
}

pub fn parse_managed_identity(body: &str) -> Result<BearerToken, String> {
    let decoded = decode(body).map_err(|e| format!("error parsing the json error: {e}"))?;
    let lifetime = decoded.expires_at.duration_since(SystemTime::now()).unwrap_or_default();
    let refresh_on = decoded
        .refresh_on
        .or_else(|| (lifetime > LONG_LIVED).then(|| SystemTime::now() + lifetime / 2));
    Ok(BearerToken {
        secret: decoded.raw.access_token,
        expires_at: decoded.expires_at,
        refresh_on,
    })
}

struct Decoded {
    raw: Raw,
    expires_at: SystemTime,
    refresh_on: Option<SystemTime>,
}

fn decode(body: &str) -> Result<Decoded, String> {
    let raw: Raw = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let now = SystemTime::now();
    duration_from_now(&raw.ext_expires_in, now)?;
    let refresh_on = duration_from_now(&raw.refresh_in, now)?;
    let expires_in = duration_from_now(&raw.expires_in, now)?;
    let expires_at = expires_on(&raw.expires_on)?
        .or(expires_in)
        .ok_or_else(|| "expires_in and expires_on are both missing or invalid".to_owned())?;
    Ok(Decoded {
        raw,
        expires_at,
        refresh_on,
    })
}

fn duration_from_now(value: &Value, now: SystemTime) -> Result<Option<SystemTime>, String> {
    let seconds = match value {
        Value::Null => return Ok(None),
        Value::String(text) => text
            .parse::<i64>()
            .map_err(|_| atoi_error(&Value::String(text.clone()), text))?,
        Value::Number(number) => number.as_i64().ok_or_else(|| atoi_error(value, &number.to_string()))?,
        other => return Err(atoi_error(other, &other.to_string())),
    };
    Ok(Some(match u64::try_from(seconds) {
        Ok(seconds) => now + Duration::from_secs(seconds),
        Err(_) => now - Duration::from_secs(seconds.unsigned_abs()),
    }))
}

fn atoi_error(raw: &Value, text: &str) -> String {
    format!(
        "unix time({raw}) could not be converted from string to int: strconv.Atoi: parsing {}: invalid syntax",
        crate::go_strconv::quote(text)
    )
}

fn expires_on(value: &Value) -> Result<Option<SystemTime>, String> {
    match value {
        Value::String(text) if !text.is_empty() => text
            .parse::<i64>()
            .ok()
            .map(unix)
            .or_else(|| formatted(text))
            .map(Some)
            .ok_or_else(|| format!("invalid ExpiresOn format: {text}")),
        Value::Number(number) => Ok(number.as_f64().map(|seconds| unix(seconds as i64))),
        _ => Ok(None),
    }
}

fn unix(seconds: i64) -> SystemTime {
    match u64::try_from(seconds) {
        Ok(seconds) => SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
        Err(_) => SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()),
    }
}

fn formatted(text: &str) -> Option<SystemTime> {
    EXPIRES_ON_FORMATS
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
        .map(|naive| naive.and_utc())
        .or_else(|| DateTime::parse_from_rfc3339(text).ok().map(|time| time.to_utc()))
        .map(SystemTime::from)
}

fn declined_scopes(requested: &str, granted: &str) -> Vec<String> {
    match granted.is_empty() {
        true => Vec::new(),
        false => {
            let granted: Vec<String> = granted.split(' ').map(str::to_lowercase).collect();
            vec![requested.to_owned()]
                .into_iter()
                .filter(|scope| !granted.contains(&scope.to_lowercase()))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes_accept_numbers_and_numeric_strings() {
        let grant = parse(
            r#"{"access_token":"a","expires_in":"3600","refresh_in":1800,"refresh_token":"r"}"#,
            "s",
        )
        .unwrap();

        assert!(grant.token.expires_at > SystemTime::now() + Duration::from_secs(3500));
        assert!(grant.token.refresh_on.unwrap() < grant.token.expires_at);
        assert_eq!(grant.refresh_token.as_deref(), Some("r"));
    }

    #[test]
    fn expires_on_wins_over_expires_in_and_accepts_every_go_format() {
        let unix_text = parse(r#"{"access_token":"a","expires_on":"4000000000","expires_in":10}"#, "s").unwrap();
        let unix_number = parse(r#"{"access_token":"a","expires_on":4000000000}"#, "s").unwrap();
        let slashed = parse(r#"{"access_token":"a","expires_on":"01/02/2100 03:04:05"}"#, "s").unwrap();
        let dashed = parse(r#"{"access_token":"a","expires_on":"2100-01-02 03:04:05"}"#, "s").unwrap();
        let rfc = parse(
            r#"{"access_token":"a","expires_on":"2100-01-02T03:04:05.123456789Z"}"#,
            "s",
        )
        .unwrap();

        assert_eq!(unix_text.token.expires_at, unix(4_000_000_000));
        assert_eq!(unix_number.token.expires_at, unix(4_000_000_000));
        assert_eq!(slashed.token.expires_at, dashed.token.expires_at);
        assert!(rfc.token.expires_at > unix(4_000_000_000));
    }

    #[test]
    fn managed_identity_tokens_are_not_validated_and_refresh_at_half_of_a_long_lifetime() {
        let long = parse_managed_identity(r#"{"expires_in":36000}"#).unwrap();
        let short = parse_managed_identity(r#"{"access_token":"a","expires_in":3600}"#).unwrap();

        assert_eq!(long.secret, "");
        assert!(long.refresh_on.unwrap() > SystemTime::now() + Duration::from_secs(17_000));
        assert_eq!(short.refresh_on, None);
        assert_eq!(
            parse_managed_identity("{}").err().unwrap(),
            "error parsing the json error: expires_in and expires_on are both missing or invalid"
        );
    }

    #[test]
    fn bodies_without_a_lifetime_are_decode_errors() {
        assert_eq!(
            parse("{}", "s").err().unwrap(),
            "json decode error: expires_in and expires_on are both missing or invalid\nraw message was: {}"
        );
        assert_eq!(
            parse(r#"{"expires_on":"tomorrow"}"#, "s").err().unwrap(),
            "json decode error: invalid ExpiresOn format: tomorrow\nraw message was: {\"expires_on\":\"tomorrow\"}"
        );
    }

    #[test]
    fn error_fields_and_missing_tokens_are_validated_after_decoding() {
        assert_eq!(
            parse(
                r#"{"error":"invalid_grant","error_description":"expired","expires_in":1}"#,
                "s"
            )
            .err()
            .unwrap(),
            "invalid_grant: expired"
        );
        assert_eq!(
            parse(r#"{"expires_in":1}"#, "s").err().unwrap(),
            "response is missing access_token"
        );
    }

    #[test]
    fn a_granted_scope_list_must_include_the_requested_scope() {
        let accepted = parse(
            r#"{"access_token":"a","expires_in":1,"scope":"x/.default Y/.DEFAULT"}"#,
            "y/.default",
        );
        let declined = parse(
            r#"{"access_token":"a","expires_in":1,"scope":"x/.default"}"#,
            "y/.default",
        );

        assert!(accepted.is_ok());
        assert_eq!(
            declined.err().unwrap(),
            "token response failed because declined scopes are present: y/.default"
        );
    }
}
