use hyper::HeaderMap;
use serde::Deserialize;

pub const DEFAULTS_HEADER_MAX_BYTES: usize = 8 << 10;
pub const DEFAULTS_USER_MAX_LEN: usize = 256;
pub const DEFAULTS_WORKDIR_MAX_LEN: usize = 4096;
pub const FIELD_USER: &str = "user";
pub const FIELD_WORKDIR: &str = "workdir";

const VERSION_HEADER: &str = "x-envd-version";
const HANDOVER_HEADER: &str = "x-envd-handover";
const DEFAULTS_HEADER: &str = "x-envd-defaults";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct HandoverResult {
    pub failed: bool,
    pub procs: i64,
    pub procs_failed: i64,
    pub retained: i64,
    pub retained_failed: i64,
    pub watchers: i64,
    pub watchers_failed: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EffectiveDefaults {
    pub user: String,
    pub workdir: Option<String>,
    pub fallback: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvdReport {
    pub version: Option<String>,
    pub handover: Option<HandoverResult>,
    pub defaults: Option<EffectiveDefaults>,
}

impl EnvdReport {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            version: text(headers, VERSION_HEADER).filter(|version| !version.is_empty()),
            handover: text(headers, HANDOVER_HEADER).and_then(|header| serde_json::from_str(&header).ok()),
            defaults: text(headers, DEFAULTS_HEADER).and_then(|header| {
                decode_defaults(&header)
                    .inspect_err(|e| tracing::warn!(error = %e, "could not decode the envd effective-defaults header"))
                    .ok()
            }),
        }
    }
}

impl EffectiveDefaults {
    pub fn user_for_display(&self) -> String {
        self.user.chars().take(DEFAULTS_USER_MAX_LEN).collect()
    }

    pub fn workdir_for_display(&self) -> String {
        self.workdir
            .iter()
            .flat_map(|workdir| workdir.chars())
            .take(DEFAULTS_WORKDIR_MAX_LEN)
            .collect()
    }

    pub fn mismatches(&self, sent_user: &str, sent_workdir: &str) -> Vec<&'static str> {
        [
            (FIELD_USER, !sent_user.is_empty() && self.user != sent_user),
            (
                FIELD_WORKDIR,
                !sent_workdir.is_empty() && self.workdir.as_deref().unwrap_or_default() != sent_workdir,
            ),
        ]
        .into_iter()
        .filter_map(|(field, disagrees)| disagrees.then_some(field))
        .collect()
    }

    pub fn preserves_exec_context(reported: Option<&Self>) -> bool {
        reported.is_some_and(|reported| !reported.fallback)
    }
}

pub fn decode_defaults(header: &str) -> Result<EffectiveDefaults, String> {
    match header.len() > DEFAULTS_HEADER_MAX_BYTES {
        true => Err(format!(
            "header is {} bytes, over the {DEFAULTS_HEADER_MAX_BYTES}-byte cap",
            header.len()
        )),
        false => serde_json::from_str(header).map_err(|e| e.to_string()),
    }
}

fn text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use hyper::header::HeaderValue;

    use super::*;

    fn defaults(user: &str, workdir: Option<&str>, fallback: bool) -> EffectiveDefaults {
        EffectiveDefaults {
            user: user.to_owned(),
            workdir: workdir.map(str::to_owned),
            fallback,
        }
    }

    fn padded_to(length: usize) -> String {
        let head = r#"{"user":""#;
        let tail = r#"","fallback":false}"#;
        format!("{head}{}{tail}", "u".repeat(length - head.len() - tail.len()))
    }

    #[test]
    fn mismatches_skip_values_that_were_never_sent() {
        let cases = [
            (defaults("user", Some("/opt/wd"), false), "user", "/opt/wd", vec![]),
            (defaults("root", None, true), "user", "", vec!["user"]),
            (defaults("user", None, false), "user", "/opt/wd", vec!["workdir"]),
            (defaults("root", None, true), "user", "/opt/wd", vec!["user", "workdir"]),
            (defaults("user", Some("/somewhere"), false), "user", "", vec![]),
            (defaults("root", None, true), "", "", vec![]),
            (defaults("user", None, false), "user", "", vec![]),
            (defaults("../../etc/passwd", None, false), "user", "", vec!["user"]),
        ];
        for (effective, user, workdir, want) in cases {
            assert_eq!(effective.mismatches(user, workdir), want, "{effective:?}");
        }
    }

    #[test]
    fn an_ordinary_defaults_header_decodes() {
        let effective = decode_defaults(r#"{"user":"user","workdir":"/home/user","fallback":false}"#).unwrap();

        assert_eq!(effective, defaults("user", Some("/home/user"), false));
    }

    #[test]
    fn a_header_exactly_at_the_cap_decodes() {
        let header = padded_to(DEFAULTS_HEADER_MAX_BYTES);

        assert_eq!(header.len(), DEFAULTS_HEADER_MAX_BYTES);
        assert!(decode_defaults(&header).is_ok());
    }

    #[test]
    fn a_header_one_byte_over_the_cap_is_refused() {
        let header = padded_to(DEFAULTS_HEADER_MAX_BYTES + 1);

        assert_eq!(header.len(), DEFAULTS_HEADER_MAX_BYTES + 1);
        assert!(decode_defaults(&header).is_err());
    }

    #[test]
    fn a_megabyte_header_is_refused() {
        assert!(decode_defaults(&padded_to(1 << 20)).is_err());
    }

    #[test]
    fn decoding_does_not_shorten_the_values() {
        let long = "u".repeat(DEFAULTS_USER_MAX_LEN * 2);

        let effective = decode_defaults(&format!(r#"{{"user":"{long}","fallback":false}}"#)).unwrap();

        assert_eq!(effective.user, long);
    }

    #[test]
    fn each_field_is_bounded_where_it_escapes() {
        let long = defaults(
            &"u".repeat(DEFAULTS_USER_MAX_LEN * 2),
            Some(&"d".repeat(DEFAULTS_WORKDIR_MAX_LEN * 2)),
            false,
        );
        assert_eq!(long.user_for_display().len(), DEFAULTS_USER_MAX_LEN);
        assert_eq!(long.workdir_for_display().len(), DEFAULTS_WORKDIR_MAX_LEN);

        let inside = defaults("app", Some("/opt/wd"), false);
        assert_eq!(inside.user_for_display(), "app");
        assert_eq!(inside.workdir_for_display(), "/opt/wd");

        assert!(EffectiveDefaults::default().workdir_for_display().is_empty());
    }

    #[test]
    fn malformed_json_is_refused() {
        assert!(decode_defaults(r#"{"user":"#).is_err());
    }

    #[test]
    fn the_report_reads_the_three_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(VERSION_HEADER, HeaderValue::from_static("0.7.1"));
        headers.insert(
            HANDOVER_HEADER,
            HeaderValue::from_static(r#"{"failed":true,"procs":3,"procs_failed":1}"#),
        );
        headers.insert(
            DEFAULTS_HEADER,
            HeaderValue::from_static(r#"{"user":"user","fallback":false}"#),
        );

        let report = EnvdReport::from_headers(&headers);

        assert_eq!(report.version.as_deref(), Some("0.7.1"));
        assert_eq!(
            report.handover,
            Some(HandoverResult {
                failed: true,
                procs: 3,
                procs_failed: 1,
                ..HandoverResult::default()
            })
        );
        assert_eq!(report.defaults, Some(defaults("user", None, false)));
    }

    #[test]
    fn an_envd_that_reports_nothing_leaves_the_report_empty() {
        assert_eq!(EnvdReport::from_headers(&HeaderMap::new()), EnvdReport::default());
    }

    #[test]
    fn an_undecodable_handover_or_defaults_header_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert(HANDOVER_HEADER, HeaderValue::from_static("not json"));
        headers.insert(DEFAULTS_HEADER, HeaderValue::from_static("{"));

        assert_eq!(EnvdReport::from_headers(&headers), EnvdReport::default());
    }

    #[test]
    fn the_exec_context_is_preserved_only_by_a_delivered_identity() {
        let cases = [
            (None, false),
            (Some(defaults("user", None, false)), true),
            (Some(defaults("root", None, true)), false),
            (Some(defaults("root", None, false)), true),
        ];
        for (reported, want) in cases {
            assert_eq!(EffectiveDefaults::preserves_exec_context(reported.as_ref()), want);
        }
    }
}
