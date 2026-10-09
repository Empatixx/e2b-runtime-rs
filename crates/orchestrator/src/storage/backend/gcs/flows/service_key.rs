use p256::pkcs8::DecodePrivateKey;
use ring::digest::{SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

use crate::storage::backend::pem;

pub enum ServiceKey {
    Rsa(RsaKeyPair),
    P256(p256::ecdsa::SigningKey),
    P384(p384::ecdsa::SigningKey),
}

impl ServiceKey {
    pub fn parse(private_key: &str) -> Result<Self, String> {
        let der = pem::blocks(private_key)
            .into_iter()
            .next()
            .map_or_else(|| private_key.as_bytes().to_vec(), |block| block.der);
        Self::pkcs8(&der)
            .or_else(|| RsaKeyPair::from_der(&der).ok().map(Self::Rsa))
            .or_else(|| Self::sec1(&der))
            .ok_or_else(|| super::super::key_errors::parse_key_failure(&der))
    }

    fn pkcs8(der: &[u8]) -> Option<Self> {
        RsaKeyPair::from_pkcs8(der)
            .ok()
            .map(Self::Rsa)
            .or_else(|| p256::ecdsa::SigningKey::from_pkcs8_der(der).ok().map(Self::P256))
            .or_else(|| p384::ecdsa::SigningKey::from_pkcs8_der(der).ok().map(Self::P384))
    }

    fn sec1(der: &[u8]) -> Option<Self> {
        p256::SecretKey::from_sec1_der(der)
            .ok()
            .map(|secret| Self::P256(secret.into()))
            .or_else(|| {
                p384::SecretKey::from_sec1_der(der)
                    .ok()
                    .map(|secret| Self::P384(secret.into()))
            })
    }

    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        let hash = digest(&SHA256, message);
        match self {
            Self::Rsa(key) => {
                let mut signature = vec![0; key.public().modulus_len()];
                key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), message, &mut signature)
                    .map_err(|e| e.to_string())?;
                Ok(signature)
            }
            Self::P256(key) => key
                .sign_prehash(hash.as_ref())
                .map(|signature: p256::ecdsa::Signature| signature.to_der().as_bytes().to_vec())
                .map_err(|e| e.to_string()),
            Self::P384(key) => key
                .sign_prehash(hash.as_ref())
                .map(|signature: p384::ecdsa::Signature| signature.to_der().as_bytes().to_vec())
                .map_err(|e| e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use p256::ecdsa::signature::hazmat::PrehashVerifier;

    use super::*;

    const RSA: &str = include_str!("../test_key.pem");
    const P256_SEC1: &str = include_str!("../test_ec_sec1.pem");
    const P256_PKCS8: &str = include_str!("../test_ec_pkcs8.pem");
    const P384_SEC1: &str = include_str!("../test_ec384_sec1.pem");

    #[test]
    fn every_go_key_format_parses() {
        assert!(matches!(ServiceKey::parse(RSA).unwrap(), ServiceKey::Rsa(_)));
        assert!(matches!(ServiceKey::parse(P256_PKCS8).unwrap(), ServiceKey::P256(_)));
        assert!(matches!(ServiceKey::parse(P256_SEC1).unwrap(), ServiceKey::P256(_)));
        assert!(matches!(ServiceKey::parse(P384_SEC1).unwrap(), ServiceKey::P384(_)));
        assert!(
            ServiceKey::parse("nope")
                .err()
                .unwrap()
                .starts_with("failed to parse private key. Tried PKCS8, PKCS1, and EC formats.")
        );
    }

    #[test]
    fn ec_keys_sign_the_sha256_digest_as_asn1() {
        let key = ServiceKey::parse(P256_SEC1).unwrap();
        let signature = key.sign(b"message").unwrap();

        let ServiceKey::P256(signing) = &key else {
            panic!("expected a P-256 key");
        };
        let parsed = p256::ecdsa::Signature::from_der(&signature).unwrap();
        signing
            .verifying_key()
            .verify_prehash(digest(&SHA256, b"message").as_ref(), &parsed)
            .unwrap();
    }
}
