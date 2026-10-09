use serde::de::DeserializeOwned;
use serde_json::Value;

const MAX_NESTING_DEPTH: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    ObjectKey,
    ObjectValue,
    ArrayValue,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    BeginValue,
    BeginValueOrEmpty,
    BeginStringOrEmpty,
    BeginString,
    EndValue,
    EndTop,
    InString,
    InStringEscape,
    InStringUnicode(u8),
    Negative,
    NonZero,
    Zero,
    Dot,
    Fraction,
    Exponent,
    ExponentSign,
    ExponentDigits,
    Literal(&'static str, usize),
    Failed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Step {
    Continue,
    EndContainer,
    End,
    Error,
}

struct Scanner {
    state: State,
    containers: Vec<Container>,
    ended: bool,
    error: Option<String>,
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

fn quote_char(c: u8) -> String {
    match c {
        b'\'' => "'\\''".to_owned(),
        b'"' => "'\"'".to_owned(),
        _ => {
            let quoted = crate::go_strconv::quote(&char::from(c).to_string());
            format!("'{}'", &quoted[1..quoted.len() - 1])
        }
    }
}

impl Scanner {
    fn new() -> Self {
        Self {
            state: State::BeginValue,
            containers: Vec::new(),
            ended: false,
            error: None,
        }
    }

    fn fail(&mut self, c: u8, context: &str) -> Step {
        self.state = State::Failed;
        self.error = Some(format!("invalid character {} {context}", quote_char(c)));
        Step::Error
    }

    fn push(&mut self, c: u8, container: Container) -> Step {
        self.containers.push(container);
        match self.containers.len() <= MAX_NESTING_DEPTH {
            true => Step::Continue,
            false => self.fail(c, "exceeded max depth"),
        }
    }

    fn pop(&mut self) {
        self.containers.pop();
        match self.containers.is_empty() {
            true => {
                self.state = State::EndTop;
                self.ended = true;
            }
            false => self.state = State::EndValue,
        }
    }

    fn step(&mut self, c: u8) -> Step {
        match self.state {
            State::BeginValue => self.begin_value(c),
            State::BeginValueOrEmpty => self.begin_value_or_empty(c),
            State::BeginStringOrEmpty => self.begin_string_or_empty(c),
            State::BeginString => self.begin_string(c),
            State::EndValue => self.end_value(c),
            State::EndTop => self.end_top(c),
            State::InString => self.in_string(c),
            State::InStringEscape => self.in_string_escape(c),
            State::InStringUnicode(digits) => self.in_string_unicode(c, digits),
            State::Negative => self.negative(c),
            State::NonZero => self.non_zero(c),
            State::Zero => self.zero(c),
            State::Dot => self.after_dot(c),
            State::Fraction => self.fraction(c),
            State::Exponent => self.exponent(c),
            State::ExponentSign => self.exponent_sign(c),
            State::ExponentDigits => self.exponent_digits(c),
            State::Literal(word, index) => self.literal(c, word, index),
            State::Failed => Step::Error,
        }
    }

    fn begin_value_or_empty(&mut self, c: u8) -> Step {
        match c {
            c if is_space(c) => Step::Continue,
            b']' => self.end_value(c),
            _ => self.begin_value(c),
        }
    }

    fn begin_value(&mut self, c: u8) -> Step {
        if is_space(c) {
            return Step::Continue;
        }
        match c {
            b'{' => {
                self.state = State::BeginStringOrEmpty;
                self.push(c, Container::ObjectKey)
            }
            b'[' => {
                self.state = State::BeginValueOrEmpty;
                self.push(c, Container::ArrayValue)
            }
            b'"' => self.enter(State::InString),
            b'-' => self.enter(State::Negative),
            b'0' => self.enter(State::Zero),
            b't' => self.enter(State::Literal("true", 1)),
            b'f' => self.enter(State::Literal("false", 1)),
            b'n' => self.enter(State::Literal("null", 1)),
            b'1'..=b'9' => self.enter(State::NonZero),
            _ => self.fail(c, "looking for beginning of value"),
        }
    }

    fn enter(&mut self, state: State) -> Step {
        self.state = state;
        Step::Continue
    }

    fn begin_string_or_empty(&mut self, c: u8) -> Step {
        match c {
            c if is_space(c) => Step::Continue,
            b'}' => {
                if let Some(last) = self.containers.last_mut() {
                    *last = Container::ObjectValue;
                }
                self.end_value(c)
            }
            _ => self.begin_string(c),
        }
    }

    fn begin_string(&mut self, c: u8) -> Step {
        match c {
            c if is_space(c) => Step::Continue,
            b'"' => self.enter(State::InString),
            _ => self.fail(c, "looking for beginning of object key string"),
        }
    }

    fn end_value(&mut self, c: u8) -> Step {
        let Some(container) = self.containers.last().copied() else {
            self.state = State::EndTop;
            self.ended = true;
            return self.end_top(c);
        };
        if is_space(c) {
            return self.enter(State::EndValue);
        }
        match (container, c) {
            (Container::ObjectKey, b':') => self.settle(Container::ObjectValue, State::BeginValue),
            (Container::ObjectKey, _) => self.fail(c, "after object key"),
            (Container::ObjectValue, b',') => self.settle(Container::ObjectKey, State::BeginString),
            (Container::ObjectValue, b'}') | (Container::ArrayValue, b']') => {
                self.pop();
                Step::EndContainer
            }
            (Container::ObjectValue, _) => self.fail(c, "after object key:value pair"),
            (Container::ArrayValue, b',') => self.enter(State::BeginValue),
            (Container::ArrayValue, _) => self.fail(c, "after array element"),
        }
    }

    fn settle(&mut self, container: Container, next: State) -> Step {
        if let Some(last) = self.containers.last_mut() {
            *last = container;
        }
        self.enter(next)
    }

    fn end_top(&mut self, c: u8) -> Step {
        if !is_space(c) {
            self.fail(c, "after top-level value");
        }
        Step::End
    }

    fn in_string(&mut self, c: u8) -> Step {
        match c {
            b'"' => self.enter(State::EndValue),
            b'\\' => self.enter(State::InStringEscape),
            c if c < 0x20 => self.fail(c, "in string literal"),
            _ => Step::Continue,
        }
    }

    fn in_string_escape(&mut self, c: u8) -> Step {
        match c {
            b'b' | b'f' | b'n' | b'r' | b't' | b'\\' | b'/' | b'"' => self.enter(State::InString),
            b'u' => self.enter(State::InStringUnicode(0)),
            _ => self.fail(c, "in string escape code"),
        }
    }

    fn in_string_unicode(&mut self, c: u8, digits: u8) -> Step {
        match (c.is_ascii_hexdigit(), digits) {
            (false, _) => self.fail(c, "in \\u hexadecimal character escape"),
            (true, 3) => self.enter(State::InString),
            (true, digits) => self.enter(State::InStringUnicode(digits + 1)),
        }
    }

    fn negative(&mut self, c: u8) -> Step {
        match c {
            b'0' => self.enter(State::Zero),
            b'1'..=b'9' => self.enter(State::NonZero),
            _ => self.fail(c, "in numeric literal"),
        }
    }

    fn non_zero(&mut self, c: u8) -> Step {
        match c.is_ascii_digit() {
            true => Step::Continue,
            false => self.zero(c),
        }
    }

    fn zero(&mut self, c: u8) -> Step {
        match c {
            b'.' => self.enter(State::Dot),
            b'e' | b'E' => self.enter(State::Exponent),
            _ => self.end_value(c),
        }
    }

    fn after_dot(&mut self, c: u8) -> Step {
        match c.is_ascii_digit() {
            true => self.enter(State::Fraction),
            false => self.fail(c, "after decimal point in numeric literal"),
        }
    }

    fn fraction(&mut self, c: u8) -> Step {
        match c {
            c if c.is_ascii_digit() => Step::Continue,
            b'e' | b'E' => self.enter(State::Exponent),
            _ => self.end_value(c),
        }
    }

    fn exponent(&mut self, c: u8) -> Step {
        match c {
            b'+' | b'-' => self.enter(State::ExponentSign),
            _ => self.exponent_sign(c),
        }
    }

    fn exponent_sign(&mut self, c: u8) -> Step {
        match c.is_ascii_digit() {
            true => self.enter(State::ExponentDigits),
            false => self.fail(c, "in exponent of numeric literal"),
        }
    }

    fn exponent_digits(&mut self, c: u8) -> Step {
        match c.is_ascii_digit() {
            true => Step::Continue,
            false => self.end_value(c),
        }
    }

    fn literal(&mut self, c: u8, word: &'static str, index: usize) -> Step {
        let expected = word.as_bytes()[index];
        match (c == expected, index + 1 == word.len()) {
            (false, _) => self.fail(c, &format!("in literal {word} (expecting '{}')", char::from(expected))),
            (true, true) => self.enter(State::EndValue),
            (true, false) => self.enter(State::Literal(word, index + 1)),
        }
    }

    fn eof(&mut self) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if self.ended {
            return Ok(());
        }
        self.step(b' ');
        match (self.ended, &self.error) {
            (true, _) => Ok(()),
            (false, Some(error)) => Err(error.clone()),
            (false, None) => Err("unexpected end of JSON input".to_owned()),
        }
    }
}

pub fn syntax_error(data: &[u8]) -> Option<String> {
    let mut scanner = Scanner::new();
    if data.iter().any(|byte| scanner.step(*byte) == Step::Error) {
        return scanner.error;
    }
    scanner.eof().err()
}

pub fn decoder_error(data: &[u8]) -> Option<String> {
    let mut scanner = Scanner::new();
    for byte in data {
        match scanner.step(*byte) {
            Step::Error => return scanner.error,
            Step::End => return None,
            Step::EndContainer if scanner.ended => return None,
            Step::EndContainer | Step::Continue => {}
        }
    }
    if scanner.step(b' ') == Step::End {
        return None;
    }
    Some(match data.iter().any(|byte| !is_space(*byte)) {
        true => "unexpected EOF".to_owned(),
        false => "EOF".to_owned(),
    })
}

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Text,
    Integer(&'static str),
    Flag,
    TextList,
    TextMap,
    Seconds,
    Any,
    Object(&'static Target),
}

impl Kind {
    fn go_type(self) -> String {
        match self {
            Self::Text => "string".to_owned(),
            Self::Integer(name) => name.to_owned(),
            Self::Flag => "bool".to_owned(),
            Self::TextList => "[]string".to_owned(),
            Self::TextMap => "map[string]string".to_owned(),
            Self::Seconds => "internal.expirationTime".to_owned(),
            Self::Any => "json.RawMessage".to_owned(),
            Self::Object(target) => target.go_type.to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub json: &'static str,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub go_type: &'static str,
    pub struct_name: &'static str,
    pub fields: &'static [Field],
}

fn value_word(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_text(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Null)
}

fn accepts(kind: Kind, value: &Value) -> bool {
    match (kind, value) {
        (_, Value::Null) | (Kind::Any, _) => true,
        (Kind::Text, Value::String(_)) | (Kind::Flag, Value::Bool(_)) | (Kind::Object(_), Value::Object(_)) => true,
        (Kind::Seconds, Value::Number(_) | Value::String(_)) => true,
        (Kind::Integer(_), Value::Number(number)) => number.is_i64() || number.is_u64(),
        (Kind::TextList, Value::Array(items)) => items.iter().all(is_text),
        (Kind::TextMap, Value::Object(entries)) => entries.values().all(is_text),
        _ => false,
    }
}

fn mismatch(kind: Kind, value: &Value) -> (String, String) {
    match (kind, value) {
        (Kind::TextList, Value::Array(items)) => (first_non_text(items.iter()), "string".to_owned()),
        (Kind::TextMap, Value::Object(entries)) => (first_non_text(entries.values()), "string".to_owned()),
        (Kind::Integer(name), Value::Number(number)) => (format!("number {number}"), name.to_owned()),
        _ => (value_word(value).to_owned(), kind.go_type()),
    }
}

fn first_non_text<'a>(mut values: impl Iterator<Item = &'a Value>) -> String {
    values
        .find(|value| !is_text(value))
        .map_or("array", value_word)
        .to_owned()
}

fn mismatch_text(target: Target, path: &str, field: Field, value: &Value) -> String {
    let (word, go_type) = mismatch(field.kind, value);
    format!(
        "json: cannot unmarshal {word} into Go struct field {}.{path}{} of type {go_type}",
        target.struct_name, field.json
    )
}

fn field_error(target: Target, path: &str, entries: &serde_json::Map<String, Value>) -> Option<String> {
    target.fields.iter().find_map(|field| {
        let found = entries.get(field.json)?;
        match (field.kind, found) {
            (kind, found) if !accepts(kind, found) => Some(mismatch_text(target, path, *field, found)),
            (Kind::Object(inner), Value::Object(inner_entries)) => {
                field_error(*inner, &format!("{path}{}.", field.json), inner_entries)
            }
            _ => None,
        }
    })
}

fn type_error(target: Target, value: &Value) -> Option<String> {
    match value {
        Value::Object(entries) => field_error(target, "", entries),
        other => Some(format!(
            "json: cannot unmarshal {} into Go value of type {}",
            value_word(other),
            target.go_type
        )),
    }
}

fn parse_value(data: &[u8], first_only: bool) -> Result<Value, String> {
    let text = String::from_utf8_lossy(data);
    match first_only {
        true => serde_json::Deserializer::from_str(&text)
            .into_iter::<Value>()
            .next()
            .ok_or_else(|| "EOF".to_owned())?
            .map_err(|e| e.to_string()),
        false => serde_json::from_str(&text).map_err(|e| e.to_string()),
    }
}

pub fn unmarshal<T: DeserializeOwned>(data: &[u8], target: Target) -> Result<T, String> {
    if let Some(error) = syntax_error(data) {
        return Err(error);
    }
    typed(parse_value(data, false)?, target)
}

pub fn decode<T: DeserializeOwned>(data: &[u8], target: Target) -> Result<T, String> {
    if let Some(error) = decoder_error(data) {
        return Err(error);
    }
    typed(parse_value(data, true)?, target)
}

fn typed<T: DeserializeOwned>(value: Value, target: Target) -> Result<T, String> {
    let value = match value {
        Value::Null => Value::Object(serde_json::Map::new()),
        other => normalize(other, target),
    };
    if let Some(error) = type_error(target, &value) {
        return Err(error);
    }
    serde_json::from_value(without_nulls(value)).map_err(|e| e.to_string())
}

fn normalize(value: Value, target: Target) -> Value {
    match value {
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, entry)| match known_field(target, &key) {
                    Some(field) => (field.json.to_owned(), normalized_entry(field.kind, entry)),
                    None => (key, entry),
                })
                .collect(),
        ),
        other => other,
    }
}

fn normalized_entry(kind: Kind, entry: Value) -> Value {
    match kind {
        Kind::Object(inner) => normalize(entry, *inner),
        _ => entry,
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

    #[test]
    fn syntax_errors_read_like_go() {
        let cases: [(&str, &str); 14] = [
            ("", "unexpected end of JSON input"),
            ("{", "unexpected end of JSON input"),
            ("x", "invalid character 'x' looking for beginning of value"),
            ("nope", "invalid character 'o' in literal null (expecting 'u')"),
            ("{\"a\" 1}", "invalid character '1' after object key"),
            ("{\"a\":1 \"b\"}", "invalid character '\"' after object key:value pair"),
            (
                "{1:2}",
                "invalid character '1' looking for beginning of object key string",
            ),
            ("[1 2]", "invalid character '2' after array element"),
            ("{} x", "invalid character 'x' after top-level value"),
            ("\"a\nb\"", "invalid character '\\n' in string literal"),
            ("\"\\q\"", "invalid character 'q' in string escape code"),
            (
                "\"\\u12g4\"",
                "invalid character 'g' in \\u hexadecimal character escape",
            ),
            ("-x", "invalid character 'x' in numeric literal"),
            ("1.x", "invalid character 'x' after decimal point in numeric literal"),
        ];

        for (input, expected) in cases {
            assert_eq!(syntax_error(input.as_bytes()).as_deref(), Some(expected), "{input:?}");
        }
        assert_eq!(
            syntax_error(b"1e"),
            Some("invalid character ' ' in exponent of numeric literal".to_owned())
        );
        assert_eq!(
            syntax_error(b"1ex"),
            Some("invalid character 'x' in exponent of numeric literal".to_owned())
        );
        assert_eq!(syntax_error(b" {\"a\": [1, 2.5e3, true, null, \"x\"]} "), None);
        assert_eq!(
            syntax_error(b"'"),
            Some("invalid character '\\'' looking for beginning of value".to_owned())
        );
        assert_eq!(syntax_error(b"\""), Some("unexpected end of JSON input".to_owned()));
    }

    #[test]
    fn decoders_stop_after_the_first_value_and_report_eof_variants() {
        assert_eq!(decoder_error(b""), Some("EOF".to_owned()));
        assert_eq!(decoder_error(b"  "), Some("EOF".to_owned()));
        assert_eq!(decoder_error(b"{\"a\":"), Some("unexpected EOF".to_owned()));
        assert_eq!(decoder_error(b"{} trailing garbage"), None);
        assert_eq!(decoder_error(b"5"), None);
        assert_eq!(
            decoder_error(b"x"),
            Some("invalid character 'x' looking for beginning of value".to_owned())
        );
    }

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
        go_type: "pkg.Inner",
        struct_name: "Inner",
        fields: &[Field {
            json: "secs",
            kind: Kind::Integer("int"),
        }],
    };

    const SAMPLE: Target = Target {
        go_type: "pkg.Sample",
        struct_name: "Sample",
        fields: &[
            Field {
                json: "name",
                kind: Kind::Text,
            },
            Field {
                json: "count",
                kind: Kind::Integer("int"),
            },
            Field {
                json: "tags",
                kind: Kind::TextList,
            },
            Field {
                json: "nest",
                kind: Kind::Object(&INNER),
            },
        ],
    };

    #[test]
    fn type_mismatches_name_the_struct_and_field() {
        let error = |json: &str| unmarshal::<Sample>(json.as_bytes(), SAMPLE).err().unwrap();

        assert_eq!(
            error(r#"{"name":5}"#),
            "json: cannot unmarshal number into Go struct field Sample.name of type string"
        );
        assert_eq!(
            error(r#"{"count":"x"}"#),
            "json: cannot unmarshal string into Go struct field Sample.count of type int"
        );
        assert_eq!(
            error(r#"{"count":1.5}"#),
            "json: cannot unmarshal number 1.5 into Go struct field Sample.count of type int"
        );
        assert_eq!(
            error(r#"{"tags":[1]}"#),
            "json: cannot unmarshal number into Go struct field Sample.tags of type string"
        );
        assert_eq!(
            error(r#"{"tags":"x"}"#),
            "json: cannot unmarshal string into Go struct field Sample.tags of type []string"
        );
        assert_eq!(
            error(r#"{"nest":{"secs":"x"}}"#),
            "json: cannot unmarshal string into Go struct field Inner.nest.secs of type int"
        );
        assert_eq!(
            error(r#"{"nest":5}"#),
            "json: cannot unmarshal number into Go struct field Sample.nest of type pkg.Inner"
        );
        assert_eq!(
            error("[]"),
            "json: cannot unmarshal array into Go value of type pkg.Sample"
        );
        assert_eq!(
            error("5"),
            "json: cannot unmarshal number into Go value of type pkg.Sample"
        );
    }

    #[test]
    fn field_names_match_case_insensitively_like_encoding_json() {
        let sample = unmarshal::<Sample>(br#"{"NAME":"n","Nest":{"SECS":4}}"#, SAMPLE).unwrap();

        assert_eq!(sample.name, "n");
        assert_eq!(sample.nest.unwrap().secs, 4);
        assert_eq!(
            unmarshal::<Sample>(br#"{"COUNT":"x"}"#, SAMPLE).unwrap_err(),
            "json: cannot unmarshal string into Go struct field Sample.count of type int"
        );
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
