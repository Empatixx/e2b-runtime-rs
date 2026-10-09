use base64::Engine;
use base64::engine::general_purpose::STANDARD;

const BEGIN: &str = "-----BEGIN ";
const END: &str = "-----END ";
const DASHES: &str = "-----";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    pub label: String,
    pub der: Vec<u8>,
}

pub fn blocks(text: &str) -> Vec<PemBlock> {
    text.split(BEGIN)
        .skip(1)
        .filter_map(|section| {
            let (label, rest) = section.split_once(DASHES)?;
            let body = rest.split_once(END)?.0;
            let encoded: String = body.split_whitespace().collect();
            Some(PemBlock {
                label: label.to_owned(),
                der: STANDARD.decode(encoded).ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_split_by_label_and_decoded() {
        let text = "junk\n-----BEGIN CERTIFICATE-----\nYWJj\n-----END CERTIFICATE-----\n-----BEGIN PRIVATE KEY-----\nZGVm\n-----END PRIVATE KEY-----\n";

        assert_eq!(
            blocks(text),
            vec![
                PemBlock {
                    label: "CERTIFICATE".to_owned(),
                    der: b"abc".to_vec()
                },
                PemBlock {
                    label: "PRIVATE KEY".to_owned(),
                    der: b"def".to_vec()
                },
            ]
        );
        assert!(blocks("nothing").is_empty());
    }
}
