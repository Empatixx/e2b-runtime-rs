use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub json: &'static str,
    pub nested: Option<&'static Target>,
}

#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub fields: &'static [Field],
}

pub fn unmarshal<T: DeserializeOwned>(data: &[u8], target: Target) -> Result<T, String> {
    serde_json::from_str(&String::from_utf8_lossy(data))
        .map_err(|e| e.to_string())
        .and_then(|value| typed(value, target))
}

pub fn decode<T: DeserializeOwned>(data: &[u8], target: Target) -> Result<T, String> {
    serde_json::Deserializer::from_str(&String::from_utf8_lossy(data))
        .into_iter::<Value>()
        .next()
        .ok_or_else(|| "empty JSON input".to_owned())?
        .map_err(|e| e.to_string())
        .and_then(|value| typed(value, target))
}

fn typed<T: DeserializeOwned>(value: Value, target: Target) -> Result<T, String> {
    let value = match value {
        Value::Null => Value::Object(Map::new()),
        Value::Object(entries) => normalize(Value::Object(entries), target),
        _ => return Err("expected a JSON object".to_owned()),
    };
    serde_json::from_value(without_nulls(value)).map_err(|e| e.to_string())
}

fn normalize(value: Value, target: Target) -> Value {
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, entry)| match known_field(target, &key) {
                    Some(field) => (field.json.to_owned(), normalized_entry(field, entry)),
                    None => (key, entry),
                })
                .collect(),
        ),
        other => other,
    }
}

fn normalized_entry(field: &Field, entry: Value) -> Value {
    match field.nested {
        Some(inner) => normalize(entry, *inner),
        None => entry,
    }
}

fn known_field(target: Target, key: &str) -> Option<&'static Field> {
    target
        .fields
        .iter()
        .find(|field| field.json == key)
        .or_else(|| target.fields.iter().find(|field| field.json.eq_ignore_ascii_case(key)))
}

fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter(|(_, entry)| !entry.is_null())
                .map(|(key, entry)| (key, without_nulls(entry)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| match item {
                    Value::Null => Value::String(String::new()),
                    other => without_nulls(other),
                })
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize, Default, Debug)]
    #[serde(default)]
    struct Sample {
        name: String,
        count: i64,
        tags: Vec<String>,
        nest: Option<Inner>,
    }

    #[derive(Deserialize, Default, Debug)]
    #[serde(default)]
    struct Inner {
        secs: i64,
    }

    const INNER: Target = Target {
        fields: &[Field {
            json: "secs",
            nested: None,
        }],
    };

    const SAMPLE: Target = Target {
        fields: &[
            Field {
                json: "name",
                nested: None,
            },
            Field {
                json: "count",
                nested: None,
            },
            Field {
                json: "tags",
                nested: None,
            },
            Field {
                json: "nest",
                nested: Some(&INNER),
            },
        ],
    };

    #[test]
    fn type_mismatches_are_errors() {
        let failed = |json: &str| unmarshal::<Sample>(json.as_bytes(), SAMPLE).is_err();

        assert!(failed(r#"{"name":5}"#));
        assert!(failed(r#"{"count":1.5}"#));
        assert!(failed(r#"{"nest":{"secs":"x"}}"#));
        assert!(failed("[]"));
        assert!(failed("{"));
    }

    #[test]
    fn field_names_match_case_insensitively() {
        let sample = unmarshal::<Sample>(br#"{"NAME":"n","Nest":{"SECS":4}}"#, SAMPLE).unwrap();

        assert_eq!(sample.name, "n");
        assert_eq!(sample.nest.unwrap().secs, 4);
    }

    #[test]
    fn a_null_document_is_an_empty_value() {
        assert_eq!(unmarshal::<Sample>(b"null", SAMPLE).unwrap().count, 0);
    }

    #[test]
    fn valid_input_and_nulls_decode_normally() {
        let sample = unmarshal::<Sample>(br#"{"name":null,"count":3,"tags":["a"],"unknown":1}"#, SAMPLE).unwrap();

        assert_eq!((sample.name.as_str(), sample.count, sample.tags.len()), ("", 3, 1));
        assert!(decode::<Sample>(br#"{"name":"n"} trailing"#, SAMPLE).is_ok());
        assert!(unmarshal::<Sample>(b"{\"name\":\"\xff\"}", SAMPLE).is_ok());
    }
}
