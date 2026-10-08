use serde::{Serialize, Serializer};

pub fn encode<T: Serialize>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string(value).map(|json| format!("{}\n", escape_html(&json)))
}

pub fn float32<S: Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    match value.fract() == 0.0 && value.is_finite() {
        true => serializer.serialize_i64(*value as i64),
        false => serializer.serialize_f64(value.to_string().parse().unwrap_or(f64::from(*value))),
    }
}

pub fn escape_html(json: &str) -> String {
    json.chars()
        .fold(String::with_capacity(json.len()), |mut escaped, character| {
            match character {
                '<' => escaped.push_str("\\u003c"),
                '>' => escaped.push_str("\\u003e"),
                '&' => escaped.push_str("\\u0026"),
                '\u{2028}' => escaped.push_str("\\u2028"),
                '\u{2029}' => escaped.push_str("\\u2029"),
                other => escaped.push(other),
            }
            escaped
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Sample {
        name: String,
        #[serde(serialize_with = "float32")]
        percent: f32,
    }

    fn encoded(name: &str, percent: f32) -> String {
        encode(&Sample {
            name: name.to_owned(),
            percent,
        })
        .unwrap()
    }

    #[test]
    fn encodes_like_go_json_encoder() {
        assert_eq!(
            encoded("a<b>&c", 12.34),
            "{\"name\":\"a\\u003cb\\u003e\\u0026c\",\"percent\":12.34}\n"
        );
        assert_eq!(encoded("x\u{2028}", 0.0), "{\"name\":\"x\\u2028\",\"percent\":0}\n");
        assert_eq!(encoded("", 100.0), "{\"name\":\"\",\"percent\":100}\n");
        assert_eq!(encoded("", 0.1), "{\"name\":\"\",\"percent\":0.1}\n");
    }
}
