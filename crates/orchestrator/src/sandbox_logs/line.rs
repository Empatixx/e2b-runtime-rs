use std::time::SystemTime;

use serde_json::Value;

use super::{Fields, SandboxMeta};
use crate::go_time::rfc3339_nano;

pub struct LogLine<'a> {
    pub level: &'static str,
    pub time: SystemTime,
    pub message: &'a str,
    pub service: &'a str,
    pub meta: &'a SandboxMeta,
    pub fields: Fields,
}

impl LogLine<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let fixed = [
            ("level", Value::from(self.level)),
            ("timestamp", Value::from(rfc3339_nano(self.time))),
            ("message", Value::from(self.message)),
            ("service", Value::from(self.service)),
            ("internal", Value::Bool(false)),
            ("pid", Value::from(std::process::id())),
            ("logger", Value::from(self.service)),
            ("sandbox.id", Value::from(self.meta.sandbox_id.as_str())),
            ("template.id", Value::from(self.meta.template_id.as_str())),
            ("team.id", Value::from(self.meta.team_id.as_str())),
            ("instanceID", Value::from(self.meta.sandbox_id.as_str())),
            ("envID", Value::from(self.meta.template_id.as_str())),
        ];
        let pairs: Vec<String> = fixed
            .iter()
            .chain(self.fields.iter())
            .map(|(key, value)| format!("{}:{}", Value::from(*key), value))
            .collect();
        format!("{{{}}}", pairs.join(",")).into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn a_line_carries_the_zap_fields_in_zap_order() {
        let meta = SandboxMeta {
            sandbox_id: "sbx".to_owned(),
            template_id: "tpl".to_owned(),
            team_id: "team".to_owned(),
        };
        let line = LogLine {
            level: "info",
            time: UNIX_EPOCH + Duration::from_millis(1500),
            message: "Killing sandbox",
            service: "orchestration-api",
            meta: &meta,
            fields: vec![("kill_reason", Value::from("unknown"))],
        };

        let encoded = String::from_utf8(line.encode()).unwrap();

        let pid = std::process::id();
        assert_eq!(
            encoded,
            format!(
                "{{\"level\":\"info\",\"timestamp\":\"1970-01-01T00:00:01.5Z\",\"message\":\"Killing sandbox\",\"service\":\"orchestration-api\",\"internal\":false,\"pid\":{pid},\"logger\":\"orchestration-api\",\"sandbox.id\":\"sbx\",\"template.id\":\"tpl\",\"team.id\":\"team\",\"instanceID\":\"sbx\",\"envID\":\"tpl\",\"kill_reason\":\"unknown\"}}"
            )
        );
    }
}
