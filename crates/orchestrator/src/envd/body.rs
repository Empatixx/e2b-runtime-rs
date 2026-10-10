use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::Deserialize;

pub const MAX_RESULT_BODY_BYTES: usize = 1 << 20;
pub const ERROR_BODY_RUNES: usize = 100;
const UTF8_MAX_BYTES: usize = 4;
const ERROR_BODY_LIMIT: usize = ERROR_BODY_RUNES * UTF8_MAX_BYTES + 1;
const ELLIPSES: &str = "...";

pub async fn read_limited<S, E>(stream: S, limit: usize) -> Vec<u8>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    let mut stream = std::pin::pin!(stream);
    let mut body = Vec::new();
    while body.len() < limit {
        let Some(Ok(chunk)) = stream.next().await else {
            break;
        };
        body.extend(chunk.iter().take(limit - body.len()));
    }
    body
}

pub async fn error_body<S, E>(stream: S) -> String
where
    S: Stream<Item = Result<Bytes, E>>,
{
    let body = read_limited(stream, ERROR_BODY_LIMIT).await;
    truncate(&String::from_utf8_lossy(&body), ERROR_BODY_RUNES)
}

pub async fn decode_result<T, S, E>(stream: S) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
    S: Stream<Item = Result<Bytes, E>>,
{
    let body = read_limited(stream, MAX_RESULT_BODY_BYTES).await;
    T::deserialize(&mut serde_json::Deserializer::from_slice(&body)).map_err(|e| e.to_string())
}

pub fn truncate(text: &str, max_runes: usize) -> String {
    let runes: Vec<char> = text.chars().collect();
    match (runes.len() <= max_runes, max_runes <= ELLIPSES.len()) {
        (true, _) => text.to_owned(),
        (false, true) => runes.into_iter().take(max_runes).collect(),
        (false, false) => runes
            .into_iter()
            .take(max_runes - ELLIPSES.len())
            .chain(ELLIPSES.chars())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use futures::stream;

    use super::*;

    fn chunks(body: &[u8]) -> impl Stream<Item = Result<Bytes, std::io::Error>> + use<> {
        stream::iter(
            body.chunks(7)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn the_cap_does_not_change_what_a_message_keeps() {
        let bodies = [
            String::new(),
            "envd: bad request".to_owned(),
            "a".repeat(100),
            "a".repeat(10_000),
            "😀".repeat(100) + "x",
            "😀".repeat(100),
            "é".repeat(1_000),
        ];
        for body in bodies {
            assert_eq!(truncate(&body, 100), error_body(chunks(body.as_bytes())).await);
        }
        let invalid = vec![0xff; 1_000];
        assert_eq!(
            truncate(&String::from_utf8_lossy(&invalid), 100),
            error_body(chunks(&invalid)).await
        );
    }

    #[test]
    fn truncate_keeps_the_ellipses_inside_the_limit() {
        assert_eq!(truncate(&"e".repeat(200), 100), "e".repeat(97) + "...");
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[tokio::test]
    async fn a_body_past_the_limit_is_not_read_to_the_end() {
        let endless = stream::repeat_with(|| Ok::<_, std::io::Error>(Bytes::from(vec![b'e'; 32 << 10])));

        let body = read_limited(endless, ERROR_BODY_LIMIT).await;

        assert_eq!(body.len(), ERROR_BODY_LIMIT);
    }

    #[tokio::test]
    async fn a_result_within_the_cap_decodes_and_one_cut_mid_value_does_not() {
        #[derive(Deserialize)]
        struct Frozen {
            frozen: i64,
        }
        let within = format!(r#"{{"frozen":3,"requested":3,"pad":"{}"}}"#, "e".repeat(64 << 10));
        let decoded: Frozen = decode_result(chunks(within.as_bytes())).await.unwrap();
        assert_eq!(decoded.frozen, 3);

        let oversized = format!(r#"{{"frozen":1,"pad":"{}"#, "e".repeat(MAX_RESULT_BODY_BYTES + 10));
        assert!(
            decode_result::<Frozen, _, _>(chunks(oversized.as_bytes()))
                .await
                .is_err()
        );
    }
}
