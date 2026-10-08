use std::collections::BTreeMap;

use super::escape::{Mode, escape, unescape};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub values: BTreeMap<String, Vec<String>>,
}

impl Query {
    pub fn parse(raw: &str) -> Self {
        let values = raw
            .split('&')
            .filter(|pair| !pair.is_empty() && !pair.contains(';'))
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                Some((
                    unescape(key, Mode::QueryComponent).ok()?,
                    unescape(value, Mode::QueryComponent).ok()?,
                ))
            })
            .fold(BTreeMap::<String, Vec<String>>::new(), |mut values, (key, value)| {
                values.entry(key).or_default().push(value);
                values
            });
        Self { values }
    }

    pub fn single(key: &str, value: &str) -> Self {
        Self {
            values: BTreeMap::from([(key.to_owned(), vec![value.to_owned()])]),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn encode(&self) -> String {
        self.values
            .iter()
            .flat_map(|(key, values)| {
                values.iter().map(move |value| {
                    format!(
                        "{}={}",
                        escape(key, Mode::QueryComponent),
                        escape(value, Mode::QueryComponent)
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("&")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_url_values() {
        let query = Query::parse("b=2&a=1&&a=x+y&bad;=1&c&d=%zz");

        assert_eq!(
            query.values,
            BTreeMap::from([
                ("a".to_owned(), vec!["1".to_owned(), "x y".to_owned()]),
                ("b".to_owned(), vec!["2".to_owned()]),
                ("c".to_owned(), vec![String::new()]),
            ])
        );
        assert_eq!(query.encode(), "a=1&a=x+y&b=2&c=");
    }
}
