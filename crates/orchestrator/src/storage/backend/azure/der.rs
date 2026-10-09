const SEQUENCE: u8 = 0x30;
const BIT_STRING: u8 = 0x03;
const CONTEXT_VERSION: u8 = 0xA0;
const RSA_ENCRYPTION: [u8; 9] = [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
const OBJECT_IDENTIFIER: u8 = 0x06;
const INTEGER: u8 = 0x02;

struct Element<'a> {
    tag: u8,
    content: &'a [u8],
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn next(&mut self) -> Option<Element<'a>> {
        let (&tag, after_tag) = self.rest.split_first()?;
        let (length, after_length) = Self::length(after_tag)?;
        let (content, rest) = after_length.split_at_checked(length)?;
        self.rest = rest;
        Some(Element { tag, content })
    }

    fn length(bytes: &[u8]) -> Option<(usize, &[u8])> {
        let (&first, rest) = bytes.split_first()?;
        match first {
            short if short < 0x80 => Some((usize::from(short), rest)),
            long => {
                let count = usize::from(long & 0x7F);
                let (digits, rest) = rest.split_at_checked(count).filter(|_| (1..=4).contains(&count))?;
                Some((digits.iter().fold(0, |acc, digit| acc << 8 | usize::from(*digit)), rest))
            }
        }
    }

    fn sequence(&mut self) -> Option<Reader<'a>> {
        self.next()
            .filter(|element| element.tag == SEQUENCE)
            .map(|element| Reader::new(element.content))
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        (0..count).try_for_each(|_| self.next().map(|_| ()))
    }
}

fn is_rsa_algorithm(mut algorithm: Reader) -> bool {
    algorithm
        .next()
        .is_some_and(|oid| oid.tag == OBJECT_IDENTIFIER && oid.content == RSA_ENCRYPTION)
}

pub fn certificate_rsa_public_key(certificate: &[u8]) -> Option<&[u8]> {
    let mut tbs = Reader::new(certificate).sequence()?.sequence()?;
    let mut field = tbs.next()?;
    if field.tag == CONTEXT_VERSION {
        field = tbs.next()?;
    }
    (field.tag == INTEGER).then_some(())?;
    tbs.skip(4)?;
    let mut info = tbs.sequence()?;
    let algorithm = info.sequence()?;
    let key = info.next().filter(|key| key.tag == BIT_STRING)?;
    is_rsa_algorithm(algorithm)
        .then(|| key.content.split_first())
        .flatten()
        .map(|(_unused_bits, public_key)| public_key)
}

pub fn pkcs8_is_rsa(private_key_info: &[u8]) -> bool {
    let Some(mut info) = Reader::new(private_key_info).sequence() else {
        return false;
    };
    info.next()
        .filter(|version| version.tag == INTEGER)
        .and_then(|_| info.sequence())
        .is_some_and(is_rsa_algorithm)
}

#[cfg(test)]
mod tests {
    use ring::signature::{KeyPair, RsaKeyPair};

    use super::super::super::pem;
    use super::*;

    const PEM: &str = include_str!("client_certificate/cert.pem");

    fn blocks() -> (Vec<u8>, Vec<u8>) {
        let blocks = pem::blocks(PEM);
        let certificate = blocks.iter().find(|block| block.label == "CERTIFICATE").unwrap();
        let key = blocks.iter().find(|block| block.label == "PRIVATE KEY").unwrap();
        (certificate.der.clone(), key.der.clone())
    }

    #[test]
    fn the_certificate_public_key_is_the_rsa_public_key_of_its_private_key() {
        let (certificate, key) = blocks();

        let pair = RsaKeyPair::from_pkcs8(&key).unwrap();

        assert_eq!(
            certificate_rsa_public_key(&certificate),
            Some(pair.public_key().as_ref())
        );
    }

    #[test]
    fn pkcs8_keys_report_their_algorithm() {
        let (_, key) = blocks();

        assert!(pkcs8_is_rsa(&key));
        assert!(!pkcs8_is_rsa(&[
            0x30, 0x0A, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2B, 0x65, 0x70
        ]));
        assert!(!pkcs8_is_rsa(b"junk"));
    }

    #[test]
    fn truncated_certificates_have_no_public_key() {
        let (certificate, _) = blocks();

        assert_eq!(certificate_rsa_public_key(&certificate[..40]), None);
    }
}
