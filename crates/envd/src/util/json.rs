use serde::Serializer;

pub fn float32<S: Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    match value.fract() == 0.0 && value.is_finite() {
        true => serializer.serialize_i64(*value as i64),
        false => serializer.serialize_f64(value.to_string().parse().unwrap_or(f64::from(*value))),
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::*;

    #[derive(Serialize)]
    struct Sample {
        #[serde(serialize_with = "float32")]
        percent: f32,
    }

    fn encoded(percent: f32) -> String {
        serde_json::to_string(&Sample { percent }).unwrap()
    }

    #[test]
    fn whole_floats_have_no_fraction_and_others_keep_their_short_form() {
        assert_eq!(encoded(0.0), "{\"percent\":0}");
        assert_eq!(encoded(100.0), "{\"percent\":100}");
        assert_eq!(encoded(12.34), "{\"percent\":12.34}");
        assert_eq!(encoded(0.1), "{\"percent\":0.1}");
    }
}
