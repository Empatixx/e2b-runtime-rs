use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmdsMetadata {
    pub sandbox_id: String,
    pub template_id: String,
    pub logs_collector_address: String,
    pub access_token_hash: Option<String>,
}

impl MmdsMetadata {
    pub fn to_json(&self) -> Value {
        let fields = [
            ("instanceID", Some(&self.sandbox_id)),
            ("envID", Some(&self.template_id)),
            ("address", Some(&self.logs_collector_address)),
            ("accessTokenHash", self.access_token_hash.as_ref()),
        ];
        let present = fields
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key.to_owned(), Value::from(value.as_str()))))
            .collect::<Map<_, _>>();
        Value::Object(present)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn metadata(access_token_hash: Option<&str>) -> MmdsMetadata {
        MmdsMetadata {
            sandbox_id: "sbx".into(),
            template_id: "tpl".into(),
            logs_collector_address: String::new(),
            access_token_hash: access_token_hash.map(str::to_owned),
        }
    }

    #[test]
    fn metadata_uses_upstream_keys_and_omits_an_empty_hash() {
        assert_eq!(
            metadata(Some("ab")).to_json(),
            json!({"instanceID": "sbx", "envID": "tpl", "address": "", "accessTokenHash": "ab"})
        );
        assert_eq!(
            metadata(None).to_json(),
            json!({"instanceID": "sbx", "envID": "tpl", "address": ""})
        );
    }
}
