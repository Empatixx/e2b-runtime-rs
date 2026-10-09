const BEGIN: &str = "-----BEGIN ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    pub label: String,
    pub der: Vec<u8>,
}

pub fn blocks(text: &str) -> Vec<PemBlock> {
    text.match_indices(BEGIN)
        .filter_map(|(start, _)| ::pem::parse(&text[start..]).ok())
        .map(|block| PemBlock {
            label: block.tag().to_owned(),
            der: block.into_contents(),
        })
        .collect()
}

pub fn first_der_or_raw(text: &str) -> Vec<u8> {
    blocks(text)
        .into_iter()
        .next()
        .map_or_else(|| text.as_bytes().to_vec(), |block| block.der)
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
