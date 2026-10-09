use p224::pkcs8::DecodePrivateKey as _;
use p256::pkcs8::DecodePrivateKey as _;
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs8::PrivateKeyInfo;
use rsa::pkcs8::spki::ObjectIdentifier;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, pkcs1v15};
use sha2::{Digest, Sha256};

const ED25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
const X25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.110");
const OCTET_STRING: u8 = 0x04;
const OKP_SEED_LENGTH: usize = 32;
const MINIMUM_RSA_BITS: usize = 1024;
const P521_FIELD_BYTES: usize = 66;
const ED25519_REFUSES_SHA256: &str =
    "ed25519: expected opts.HashFunc() zero (unhashed message, for standard Ed25519) or SHA-512 (for Ed25519ph)";

pub enum PrivateKey {
    Rsa(RsaPrivateKey),
    P224(p224::ecdsa::SigningKey),
    P256(p256::ecdsa::SigningKey),
    P384(p384::ecdsa::SigningKey),
    P521(p521::ecdsa::SigningKey),
    Ed25519,
    X25519,
}

impl PrivateKey {
    pub fn pkcs8(der: &[u8]) -> Option<Self> {
        RsaPrivateKey::from_pkcs8_der(der)
            .ok()
            .map(Self::Rsa)
            .or_else(|| Self::curve_pkcs8(der))
            .or_else(|| Self::okp_pkcs8(der))
    }

    pub fn pkcs1(der: &[u8]) -> Option<Self> {
        RsaPrivateKey::from_pkcs1_der(der).ok().map(Self::Rsa)
    }

    pub fn sec1(der: &[u8]) -> Option<Self> {
        p256::SecretKey::from_sec1_der(der)
            .ok()
            .map(|secret| Self::P256(secret.into()))
            .or_else(|| {
                p384::SecretKey::from_sec1_der(der)
                    .ok()
                    .map(|secret| Self::P384(secret.into()))
            })
            .or_else(|| {
                p521::SecretKey::from_sec1_der(der)
                    .ok()
                    .map(|secret| Self::P521(secret.into()))
            })
            .or_else(|| {
                p224::SecretKey::from_sec1_der(der)
                    .ok()
                    .map(|secret| Self::P224(secret.into()))
            })
    }

    fn curve_pkcs8(der: &[u8]) -> Option<Self> {
        p256::ecdsa::SigningKey::from_pkcs8_der(der)
            .ok()
            .map(Self::P256)
            .or_else(|| p384::ecdsa::SigningKey::from_pkcs8_der(der).ok().map(Self::P384))
            .or_else(|| p521::ecdsa::SigningKey::from_pkcs8_der(der).ok().map(Self::P521))
            .or_else(|| p224::ecdsa::SigningKey::from_pkcs8_der(der).ok().map(Self::P224))
    }

    fn okp_pkcs8(der: &[u8]) -> Option<Self> {
        let info = PrivateKeyInfo::try_from(der).ok()?;
        let wrapped_seed = info.private_key.len() == OKP_SEED_LENGTH + 2
            && info.private_key.first() == Some(&OCTET_STRING)
            && info.private_key.get(1) == Some(&(OKP_SEED_LENGTH as u8));
        match (wrapped_seed, info.algorithm.parameters.is_none()) {
            (true, true) if info.algorithm.oid == ED25519 => Some(Self::Ed25519),
            (true, true) if info.algorithm.oid == X25519 => Some(Self::X25519),
            _ => None,
        }
    }

    pub fn is_rsa(&self) -> bool {
        matches!(self, Self::Rsa(_))
    }

    pub fn is_signer(&self) -> bool {
        !matches!(self, Self::X25519)
    }

    pub fn into_rsa(self) -> Option<RsaPrivateKey> {
        match self {
            Self::Rsa(key) => Some(key),
            _ => None,
        }
    }

    pub fn sign_sha256(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        let digest = Sha256::digest(message);
        match self {
            Self::Rsa(key) => sign_rsa(key, message),
            Self::P224(key) => {
                use p224::ecdsa::signature::hazmat::PrehashSigner;
                key.sign_prehash(&digest)
                    .map(|signature: p224::ecdsa::Signature| signature.to_der().as_bytes().to_vec())
                    .map_err(|e| e.to_string())
            }
            Self::P256(key) => {
                use p256::ecdsa::signature::hazmat::PrehashSigner;
                key.sign_prehash(&digest)
                    .map(|signature: p256::ecdsa::Signature| signature.to_der().as_bytes().to_vec())
                    .map_err(|e| e.to_string())
            }
            Self::P384(key) => {
                use p384::ecdsa::signature::hazmat::PrehashSigner;
                key.sign_prehash(&digest)
                    .map(|signature: p384::ecdsa::Signature| signature.to_der().as_bytes().to_vec())
                    .map_err(|e| e.to_string())
            }
            Self::P521(key) => {
                use p521::ecdsa::signature::hazmat::PrehashSigner;
                let mut padded = vec![0; P521_FIELD_BYTES - digest.len()];
                padded.extend_from_slice(&digest);
                key.sign_prehash(&padded)
                    .map(|signature: p521::ecdsa::Signature| signature.to_der().as_bytes().to_vec())
                    .map_err(|e| e.to_string())
            }
            Self::Ed25519 => Err(ED25519_REFUSES_SHA256.to_owned()),
            Self::X25519 => Err("private key is not a signer".to_owned()),
        }
    }
}

pub fn sign_rsa(key: &RsaPrivateKey, message: &[u8]) -> Result<Vec<u8>, String> {
    let bits = key.n().bits();
    if bits < MINIMUM_RSA_BITS {
        return Err(format!(
            "crypto/rsa: {bits}-bit keys are insecure (see https://go.dev/pkg/crypto/rsa#hdr-Minimum_key_size)"
        ));
    }
    pkcs1v15::SigningKey::<Sha256>::new(key.clone())
        .try_sign(message)
        .map(|signature| signature.to_vec())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::backend::pem;

    fn der(pem_text: &str) -> Vec<u8> {
        pem::blocks(pem_text).remove(0).der
    }

    #[test]
    fn rsa_keys_parse_from_both_containers_and_sign() {
        let pkcs8 = der(include_str!("test_key.pem"));

        let key = PrivateKey::pkcs8(&pkcs8).unwrap();

        assert!(key.is_rsa());
        assert_eq!(key.sign_sha256(b"message").unwrap().len(), 256);
    }

    #[test]
    fn ec_keys_of_every_go_curve_sign_as_der() {
        for text in [
            include_str!("test_ec_pkcs8.pem"),
            include_str!("test_ec_sec1.pem"),
            include_str!("test_ec384_sec1.pem"),
            include_str!("test_ec224_pkcs8.pem"),
            include_str!("test_ec224_sec1.pem"),
            include_str!("test_ec521_pkcs8.pem"),
            include_str!("test_ec521_sec1.pem"),
        ] {
            let der = der(text);
            let key = PrivateKey::pkcs8(&der).or_else(|| PrivateKey::sec1(&der)).unwrap();

            let signature = key.sign_sha256(b"message").unwrap();

            assert_eq!(signature[0], 0x30, "an ASN.1 sequence");
        }
    }

    #[test]
    fn ed25519_parses_but_refuses_sha256_and_x25519_is_no_signer() {
        let ed = PrivateKey::pkcs8(&der(include_str!("test_ed25519.pem"))).unwrap();
        let x = PrivateKey::pkcs8(&der(include_str!("test_x25519.pem"))).unwrap();

        assert_eq!(ed.sign_sha256(b"m").unwrap_err(), ED25519_REFUSES_SHA256);
        assert!(!x.is_signer());
    }

    #[test]
    fn small_rsa_keys_are_refused_at_signing_like_go() {
        let small = PrivateKey::pkcs8(&der(include_str!("test_rsa768.pem"))).unwrap();
        let ok = PrivateKey::pkcs8(&der(include_str!("test_rsa1024.pem"))).unwrap();

        assert_eq!(
            small.sign_sha256(b"m").unwrap_err(),
            "crypto/rsa: 768-bit keys are insecure (see https://go.dev/pkg/crypto/rsa#hdr-Minimum_key_size)"
        );
        assert_eq!(ok.sign_sha256(b"m").unwrap().len(), 128);
    }

    #[test]
    fn garbage_is_no_key() {
        assert!(PrivateKey::pkcs8(b"nope").is_none());
        assert!(PrivateKey::pkcs1(b"nope").is_none());
        assert!(PrivateKey::sec1(b"nope").is_none());
    }
}
