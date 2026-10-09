use std::collections::BTreeMap;

use reqwest::RequestBuilder;
use reqwest::header::CONTENT_TYPE;

use crate::go_url::query::Query;

const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

pub fn encode<K, V>(pairs: impl IntoIterator<Item = (K, V)>) -> String
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    let values = pairs
        .into_iter()
        .fold(BTreeMap::<String, Vec<String>>::new(), |mut values, (key, value)| {
            values
                .entry(key.as_ref().to_owned())
                .or_default()
                .push(value.as_ref().to_owned());
            values
        });
    Query { values }.encode()
}

pub trait UrlEncodedForm {
    fn url_encoded_form<K, V>(self, pairs: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>;
}

impl UrlEncodedForm for RequestBuilder {
    fn url_encoded_form<K, V>(self, pairs: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.header(CONTENT_TYPE, FORM_CONTENT_TYPE).body(encode(pairs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_sorted_like_url_values_encode() {
        assert_eq!(
            encode([("grant_type", "jwt"), ("assertion", "a b"), ("assertion", "c")]),
            "assertion=a+b&assertion=c&grant_type=jwt"
        );
    }

    #[test]
    fn empty_values_are_kept() {
        assert_eq!(encode([("scope", ""), ("audience", "x/y")]), "audience=x%2Fy&scope=");
    }
}
