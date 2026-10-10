use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    values: BTreeMap<String, String>,
    visited: BTreeSet<String>,
    pub rest: Vec<String>,
}

pub fn parse(definitions: &[(&str, Kind)], args: &[String]) -> Result<Parsed, String> {
    let kinds: BTreeMap<&str, Kind> = definitions.iter().copied().collect();
    let mut parsed = Parsed::default();
    let mut remaining = args.iter();
    while let Some(arg) = remaining.next() {
        let Some(flag) = flag_body(arg) else {
            parsed.rest.push(arg.clone());
            parsed.rest.extend(remaining.cloned());
            break;
        };
        if flag.is_empty() {
            parsed.rest.extend(remaining.cloned());
            break;
        }
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (flag, None),
        };
        let kind = kinds
            .get(name)
            .ok_or_else(|| format!("flag provided but not defined: -{name}"))?;
        let value = match (kind, inline) {
            (_, Some(value)) => value,
            (Kind::Bool, None) => "true".to_owned(),
            (Kind::Value, None) => remaining
                .next()
                .cloned()
                .ok_or_else(|| format!("flag needs an argument: -{name}"))?,
        };
        if *kind == Kind::Bool && crate::go_strconv::parse_bool(&value).is_err() {
            return Err(format!("invalid boolean value {value:?} for -{name}: parse error"));
        }
        parsed.visited.insert(name.to_owned());
        parsed.values.insert(name.to_owned(), value);
    }
    Ok(parsed)
}

fn flag_body(arg: &str) -> Option<&str> {
    match arg {
        "-" => None,
        "--" => Some(""),
        _ => arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .filter(|body| !body.starts_with('-')),
    }
}

impl Parsed {
    pub fn visited(&self, name: &str) -> bool {
        self.visited.contains(name)
    }

    pub fn string(&self, name: &str, default: &str) -> String {
        self.values.get(name).cloned().unwrap_or_else(|| default.to_owned())
    }

    pub fn bool(&self, name: &str) -> bool {
        self.values
            .get(name)
            .is_some_and(|value| crate::go_strconv::parse_bool(value).unwrap_or(false))
    }

    pub fn int(&self, name: &str) -> Result<i64, String> {
        match self.values.get(name) {
            None => Ok(0),
            Some(value) => value
                .parse()
                .map_err(|_| format!("invalid value {value:?} for flag -{name}: parse error")),
        }
    }

    pub fn duration(&self, name: &str) -> Result<std::time::Duration, String> {
        match self.values.get(name) {
            None => Ok(std::time::Duration::ZERO),
            Some(value) => crate::go_time::parse_duration(value)
                .map(|nanos| std::time::Duration::from_nanos(nanos.max(0) as u64))
                .map_err(|_| format!("invalid value {value:?} for flag -{name}: parse error")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFINITIONS: &[(&str, Kind)] = &[
        ("v", Kind::Bool),
        ("from-build", Kind::Value),
        ("iterations", Kind::Value),
    ];

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn values_take_the_next_argument_or_an_inline_one_like_go() {
        let parsed = parse(DEFINITIONS, &args(&["-from-build", "abc", "--iterations=3", "-v"])).unwrap();

        assert_eq!(parsed.string("from-build", ""), "abc");
        assert_eq!(parsed.int("iterations"), Ok(3));
        assert!(parsed.bool("v"));
        assert!(parsed.visited("from-build"));
        assert!(!parsed.visited("missing"));
    }

    #[test]
    fn a_bool_can_be_switched_off_and_parsing_stops_at_the_first_argument() {
        let parsed = parse(DEFINITIONS, &args(&["-v=false", "rest", "-v"])).unwrap();

        assert!(!parsed.bool("v"));
        assert!(parsed.visited("v"));
        assert_eq!(parsed.rest, args(&["rest", "-v"]));
    }

    #[test]
    fn errors_read_like_go() {
        assert_eq!(
            parse(DEFINITIONS, &args(&["-nope"])),
            Err("flag provided but not defined: -nope".to_owned())
        );
        assert_eq!(
            parse(DEFINITIONS, &args(&["-from-build"])),
            Err("flag needs an argument: -from-build".to_owned())
        );
        assert_eq!(
            parse(DEFINITIONS, &args(&["-iterations", "x"]))
                .unwrap()
                .int("iterations"),
            Err("invalid value \"x\" for flag -iterations: parse error".to_owned())
        );
    }
}
