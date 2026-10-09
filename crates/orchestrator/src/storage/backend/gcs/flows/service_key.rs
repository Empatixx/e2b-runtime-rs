use super::super::key_errors::parse_key_failure;
use super::super::private_key::PrivateKey;
use crate::storage::backend::pem;

const NOT_A_SIGNER: &str = "private key is not a signer";

pub struct ServiceKey {
    key: PrivateKey,
}

impl ServiceKey {
    pub fn parse(private_key: &str) -> Result<Self, String> {
        let der = pem::blocks(private_key)
            .into_iter()
            .next()
            .map_or_else(|| private_key.as_bytes().to_vec(), |block| block.der);
        let key = PrivateKey::pkcs8(&der)
            .or_else(|| PrivateKey::pkcs1(&der))
            .or_else(|| PrivateKey::sec1(&der))
            .ok_or_else(|| parse_key_failure(&der))?;
        match key.is_signer() {
            true => Ok(Self { key }),
            false => Err(NOT_A_SIGNER.to_owned()),
        }
    }

    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        self.key.sign_sha256(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSA: &str = include_str!("../test_key.pem");
    const P256_SEC1: &str = include_str!("../test_ec_sec1.pem");
    const P256_PKCS8: &str = include_str!("../test_ec_pkcs8.pem");
    const P384_SEC1: &str = include_str!("../test_ec384_sec1.pem");
    const P224_SEC1: &str = include_str!("../test_ec224_sec1.pem");
    const P521_PKCS8: &str = include_str!("../test_ec521_pkcs8.pem");
    const ED25519: &str = include_str!("../test_ed25519.pem");
    const X25519: &str = include_str!("../test_x25519.pem");

    #[test]
    fn every_go_key_format_parses_and_signs() {
        for text in [RSA, P256_PKCS8, P256_SEC1, P384_SEC1, P224_SEC1, P521_PKCS8] {
            assert!(!ServiceKey::parse(text).unwrap().sign(b"m").unwrap().is_empty());
        }
    }

    #[test]
    fn unparseable_keys_list_every_attempt() {
        assert!(
            ServiceKey::parse("nope")
                .err()
                .unwrap()
                .starts_with("failed to parse private key. Tried PKCS8, PKCS1, and EC formats.")
        );
    }

    #[test]
    fn ed25519_parses_but_cannot_sign_a_sha256_digest_and_x25519_is_no_signer() {
        assert_eq!(
            ServiceKey::parse(ED25519).unwrap().sign(b"m").unwrap_err(),
            "ed25519: expected opts.HashFunc() zero (unhashed message, for standard Ed25519) or SHA-512 (for Ed25519ph)"
        );
        assert_eq!(ServiceKey::parse(X25519).err().unwrap(), NOT_A_SIGNER);
    }
}
