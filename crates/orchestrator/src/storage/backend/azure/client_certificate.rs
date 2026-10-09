use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use p12_keystore::{KeyStore, Pkcs12ImportPolicy};
use pkcs8::der::Decode;
use pkcs8::{ObjectIdentifier, PrivateKeyInfoRef};
use reqwest::Client;
use ring::digest::{Algorithm as DigestAlgorithm, SHA1_FOR_LEGACY_USE_ONLY, SHA256, digest};
use ring::signature::RsaKeyPair;
use serde::Serialize;
use tokio::sync::Mutex;
use x509_cert::Certificate;

use super::super::pem::{self, PemBlock};
use super::aad::{Authority, Exchange};
use super::msal::{self, CacheKey, TokenCache};
use super::tenant::TenantPolicy;
use super::token_source::{BearerToken, SourceError, TokenRequest, TokenSource};

const CREDENTIAL: &str = "ClientCertificateCredential";
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const ASSERTION_LIFETIME_SECONDS: u64 = 600;
const RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");

pub struct ClientCertificate {
    signing: Vec<u8>,
    chain: Vec<Vec<u8>>,
    key: EncodingKey,
}

#[derive(Serialize)]
struct Claims<'a> {
    aud: &'a str,
    exp: u64,
    iss: &'a str,
    jti: String,
    nbf: u64,
    sub: &'a str,
}

pub struct ClientCertificateCredential {
    pub client_id: String,
    pub certificate: ClientCertificate,
    pub send_chain: bool,
    pub authority: Authority,
    pub tenants: TenantPolicy,
    pub client: Client,
    pub cache: Mutex<TokenCache>,
}

#[derive(Clone, Copy)]
pub enum Signing {
    Pss256,
    Rs256,
}

impl Signing {
    fn of(authority: &Authority) -> Self {
        match authority.is_adfs() {
            true => Self::Rs256,
            false => Self::Pss256,
        }
    }

    fn header(self, certificate: &[u8]) -> Header {
        match self {
            Self::Pss256 => Header {
                x5t_s256: Some(thumbprint(&SHA256, certificate)),
                ..Header::new(Algorithm::PS256)
            },
            Self::Rs256 => Header {
                x5t: Some(thumbprint(&SHA1_FOR_LEGACY_USE_ONLY, certificate)),
                ..Header::new(Algorithm::RS256)
            },
        }
    }
}

fn thumbprint(algorithm: &'static DigestAlgorithm, certificate: &[u8]) -> String {
    STANDARD.encode(digest(algorithm, certificate))
}

impl ClientCertificate {
    pub fn load(path: &str, password: &str) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| {
            format!(
                "failed to read certificate file \"{path}\": {}",
                crate::go_os::path_error("open", Path::new(path), &e)
            )
        })?;
        Self::parse(&data, password).map_err(|e| {
            format!(
                "failed to parse {} due to error {}. This may be due to a limitation of this module's certificate loader. Consider calling NewClientCertificateCredential instead",
                crate::go_strconv::quote(path),
                crate::go_strconv::quote(&e)
            )
        })
    }

    pub fn parse(data: &[u8], password: &str) -> Result<Self, String> {
        let blocks = match password.is_empty() {
            true => pem::blocks(&String::from_utf8_lossy(data)),
            false => Vec::new(),
        };
        let blocks = match blocks.is_empty() {
            true => pkcs12_blocks(data, password)?,
            false => blocks,
        };
        Self::from_blocks(&blocks)
    }

    fn from_blocks(blocks: &[PemBlock]) -> Result<Self, String> {
        let certificates: Vec<&[u8]> = blocks
            .iter()
            .filter(|block| block.label == "CERTIFICATE")
            .map(|block| block.der.as_slice())
            .collect();
        let mut keys = blocks
            .iter()
            .filter(|block| block.label == "PRIVATE KEY" || block.label == "RSA PRIVATE KEY");
        let key = keys.next();
        if keys.next().is_some() {
            return Err("certData contains multiple private keys".to_owned());
        }
        if certificates.is_empty() {
            return Err("found no certificate".to_owned());
        }
        let key = key.ok_or_else(|| "found no private key".to_owned())?;
        let public = rsa_key_pair(key)?.public().as_ref().to_vec();
        Self::matching(&certificates, &public, rsa_encoding_key(&key.der))
    }

    fn matching(certificates: &[&[u8]], public: &[u8], key: EncodingKey) -> Result<Self, String> {
        let matches = |certificate: &&[u8]| certificate_rsa_public_key(certificate).as_deref() == Some(public);
        let signing = certificates
            .iter()
            .rev()
            .find(|certificate| matches(certificate))
            .ok_or_else(|| "key doesn't match any certificate".to_owned())?;
        let chain = certificates
            .iter()
            .filter(|certificate| matches(certificate))
            .rev()
            .chain(certificates.iter().filter(|certificate| !matches(certificate)))
            .map(|certificate| certificate.to_vec())
            .collect();
        Ok(Self {
            signing: signing.to_vec(),
            chain,
            key,
        })
    }

    pub fn assertion(
        &self,
        audience: &str,
        client_id: &str,
        send_chain: bool,
        now_unix: u64,
        signing: Signing,
    ) -> Result<String, String> {
        let header = Header {
            x5c: send_chain.then(|| {
                self.chain
                    .iter()
                    .map(|certificate| STANDARD.encode(certificate))
                    .collect()
            }),
            ..signing.header(&self.signing)
        };
        let claims = Claims {
            aud: audience,
            exp: now_unix + ASSERTION_LIFETIME_SECONDS,
            iss: client_id,
            jti: uuid::Uuid::new_v4().to_string(),
            nbf: now_unix,
            sub: client_id,
        };
        jsonwebtoken::encode(&header, &claims, &self.key).map_err(|e| format!("unable to sign JWT token: {e}"))
    }
}

fn rsa_key_pair(key: &PemBlock) -> Result<RsaKeyPair, String> {
    if key.label == "PRIVATE KEY" && !pkcs8_is_rsa(&key.der) && RsaKeyPair::from_der(&key.der).is_err() {
        return Err("key must be an RSA key".to_owned());
    }
    RsaKeyPair::from_pkcs8(&key.der)
        .or_else(|_| RsaKeyPair::from_der(&key.der))
        .map_err(|e| e.to_string())
}

fn certificate_rsa_public_key(certificate: &[u8]) -> Option<Vec<u8>> {
    Certificate::from_der(certificate)
        .ok()
        .map(|certificate| certificate.tbs_certificate().subject_public_key_info().clone())
        .filter(|info| info.algorithm.oid == RSA_ENCRYPTION)
        .and_then(|info| info.subject_public_key.as_bytes().map(<[u8]>::to_vec))
}

fn rsa_encoding_key(der: &[u8]) -> EncodingKey {
    EncodingKey::from_rsa_der(PrivateKeyInfoRef::from_der(der).map_or(der, |info| info.private_key.as_bytes()))
}

fn pkcs8_is_rsa(private_key_info: &[u8]) -> bool {
    PrivateKeyInfoRef::from_der(private_key_info).is_ok_and(|info| info.algorithm.oid == RSA_ENCRYPTION)
}

fn pkcs12_blocks(data: &[u8], password: &str) -> Result<Vec<PemBlock>, String> {
    let store = KeyStore::from_pkcs12(data, password, Pkcs12ImportPolicy::Relaxed).map_err(|e| e.to_string())?;
    let (_, chain) = store
        .private_key_chain()
        .ok_or_else(|| "found no private key".to_owned())?;
    Ok(chain
        .certs()
        .iter()
        .map(|cert| PemBlock {
            label: "CERTIFICATE".to_owned(),
            der: cert.as_der().to_vec(),
        })
        .chain([PemBlock {
            label: "PRIVATE KEY".to_owned(),
            der: chain.key().as_der().to_vec(),
        }])
        .collect())
}

impl ClientCertificateCredential {
    async fn acquire(&self, authority: &Authority, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let endpoints = authority
            .resolve(&self.client)
            .await
            .map_err(|e| e.failed(CREDENTIAL, |message| message))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let assertion = self
            .certificate
            .assertion(
                &endpoints.token_endpoint,
                &self.client_id,
                self.send_chain,
                now,
                Signing::of(authority),
            )
            .map_err(|e| SourceError::Failed(format!("{CREDENTIAL}: {e}")))?;
        endpoints
            .exchange(
                &self.client,
                Exchange {
                    credential: CREDENTIAL,
                    scope: request.scope,
                    cae: request.cae,
                    form: vec![
                        ("grant_type", "client_credentials".to_owned()),
                        ("client_assertion_type", ASSERTION_TYPE.to_owned()),
                        ("client_assertion", assertion),
                        ("client_id", self.client_id.clone()),
                        ("client_info", "1".to_owned()),
                    ],
                },
            )
            .await
            .map(|grant| grant.token)
    }
}

#[async_trait::async_trait]
impl TokenSource for ClientCertificateCredential {
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError> {
        let authority = self.authority.for_request(&self.tenants, CREDENTIAL, request.tenant)?;
        let key = CacheKey::of(request, authority.tenant());
        let mut cache = self.cache.lock().await;
        msal::confidential_token(&mut cache, key, || self.acquire(&authority, request)).await
    }
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RSA_PSS_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;

    const PEM: &[u8] = include_bytes!("client_certificate/cert.pem");
    const PKCS12: &[u8] = include_bytes!("client_certificate/cert.p12");
    const OTHER_CERTIFICATE: &str = include_str!("client_certificate/other_cert.pem");

    fn decoded(part: &str) -> String {
        String::from_utf8(URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn pem_and_pkcs12_files_yield_the_same_certificate() {
        let from_pem = ClientCertificate::parse(PEM, "").unwrap();
        let from_p12 = ClientCertificate::parse(PKCS12, "secret").unwrap();

        assert_eq!(from_pem.chain.len(), 1);
        assert_eq!(from_pem.chain, from_p12.chain);
    }

    #[test]
    fn broken_inputs_fail_with_the_sdk_texts() {
        let blocks = pem::blocks(&String::from_utf8_lossy(PEM));
        let certificate = blocks
            .iter()
            .find(|block| block.label == "CERTIFICATE")
            .unwrap()
            .clone();
        let key = blocks
            .iter()
            .find(|block| block.label != "CERTIFICATE")
            .unwrap()
            .clone();

        assert_eq!(
            ClientCertificate::from_blocks(std::slice::from_ref(&key))
                .err()
                .unwrap(),
            "found no certificate"
        );
        assert_eq!(
            ClientCertificate::from_blocks(std::slice::from_ref(&certificate))
                .err()
                .unwrap(),
            "found no private key"
        );
        assert_eq!(
            ClientCertificate::from_blocks(&[certificate, key.clone(), key])
                .err()
                .unwrap(),
            "certData contains multiple private keys"
        );
        assert!(ClientCertificate::load("/nonexistent/cert.pem", "").err().unwrap().starts_with(
            "failed to read certificate file \"/nonexistent/cert.pem\": open /nonexistent/cert.pem: no such file or directory"
        ));
    }

    #[test]
    fn assertions_are_ps256_jwts_with_the_sha256_thumbprint() {
        let certificate = ClientCertificate::parse(PEM, "").unwrap();

        let jwt = certificate
            .assertion(
                "https://login/t/oauth2/v2.0/token",
                "client",
                true,
                1_700_000_000,
                Signing::Pss256,
            )
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();

        let header = decoded(parts[0]);
        assert!(header.starts_with(r#"{"typ":"JWT","alg":"PS256","x5c":[""#));
        let thumbprint = STANDARD.encode(digest(&SHA256, &certificate.chain[0]));
        assert!(header.ends_with(&format!(r#""x5t#S256":"{thumbprint}"}}"#)));
        let claims = decoded(parts[1]);
        assert!(
            claims.starts_with(r#"{"aud":"https://login/t/oauth2/v2.0/token","exp":1700000600,"iss":"client","jti":""#)
        );
        assert!(claims.ends_with(r#"","nbf":1700000000,"sub":"client"}"#));
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PSS_2048_8192_SHA256, &public_key())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
        let without_chain = certificate.assertion("a", "c", false, 1, Signing::Pss256).unwrap();
        assert!(
            decoded(without_chain.split('.').next().unwrap()).starts_with(r#"{"typ":"JWT","alg":"PS256","x5t#S256""#)
        );
    }

    fn public_key() -> Vec<u8> {
        rsa_key_pair(&pem_blocks().1).unwrap().public().as_ref().to_vec()
    }

    fn pem_blocks() -> (PemBlock, PemBlock, PemBlock) {
        let blocks = pem::blocks(&String::from_utf8_lossy(PEM));
        let certificate = blocks
            .iter()
            .find(|block| block.label == "CERTIFICATE")
            .unwrap()
            .clone();
        let key = blocks
            .iter()
            .find(|block| block.label != "CERTIFICATE")
            .unwrap()
            .clone();
        let other = pem::blocks(OTHER_CERTIFICATE).remove(0);
        (certificate, key, other)
    }

    #[test]
    fn the_certificate_matching_the_key_signs_even_when_it_is_not_first() {
        let (certificate, key, other) = pem_blocks();

        let loaded = ClientCertificate::from_blocks(&[other.clone(), certificate.clone(), key]).unwrap();

        assert_eq!(loaded.signing, certificate.der);
        assert_eq!(loaded.chain, vec![certificate.der, other.der]);
    }

    #[test]
    fn non_rsa_pkcs8_keys_are_rejected() {
        let ed25519 = PemBlock {
            label: "PRIVATE KEY".to_owned(),
            der: vec![0x30, 0x0A, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2B, 0x65, 0x70],
        };

        assert_eq!(rsa_key_pair(&ed25519).err().unwrap(), "key must be an RSA key");
    }

    #[test]
    fn a_key_without_a_matching_certificate_is_rejected() {
        let (_, key, other) = pem_blocks();

        assert_eq!(
            ClientCertificate::from_blocks(&[other, key]).err().unwrap(),
            "key doesn't match any certificate"
        );
    }

    #[test]
    fn adfs_assertions_are_rs256_with_a_sha1_thumbprint() {
        let certificate = ClientCertificate::parse(PEM, "").unwrap();

        let jwt = certificate
            .assertion("https://adfs/token", "client", false, 1_700_000_000, Signing::Rs256)
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();

        let thumbprint = STANDARD.encode(digest(&SHA1_FOR_LEGACY_USE_ONLY, &certificate.signing));
        assert_eq!(
            decoded(parts[0]),
            format!(r#"{{"typ":"JWT","alg":"RS256","x5t":"{thumbprint}"}}"#)
        );
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public_key())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
    }

    fn credential(authority: Authority, default_tenant: &str, send_chain: bool) -> ClientCertificateCredential {
        ClientCertificateCredential {
            client_id: "client".to_owned(),
            certificate: ClientCertificate::parse(PEM, "").unwrap(),
            send_chain,
            authority,
            tenants: TenantPolicy::new(default_tenant, &[]),
            client: Client::new(),
            cache: Mutex::default(),
        }
    }

    fn token_reply() -> (u16, &'static str, String) {
        (
            200,
            "application/json",
            r#"{"access_token":"aad","expires_in":3600}"#.to_owned(),
        )
    }

    #[tokio::test]
    async fn adfs_authorities_sign_for_the_discovered_endpoint() {
        let (base, handle) = super::super::testing::serve(vec![
            (
                200,
                "application/json",
                r#"{"authorization_endpoint":"{BASE}/adfs/oauth2/authorize","token_endpoint":"{BASE}/adfs/oauth2/token","issuer":"{BASE}/adfs"}"#.to_owned(),
            ),
            token_reply(),
        ])
        .await;
        let credential = credential(Authority::local_adfs(&base), "adfs", false);

        let token = credential.token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[1].starts_with("POST /adfs/oauth2/token HTTP/1.1"));
        let assertion = requests[1]
            .split("client_assertion=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .unwrap();
        assert!(decoded(assertion.split('.').next().unwrap()).starts_with(r#"{"typ":"JWT","alg":"RS256""#));
        assert!(
            decoded(assertion.split('.').nth(1).unwrap()).contains(&format!(r#""aud":"{base}/adfs/oauth2/token""#))
        );
    }

    #[tokio::test]
    async fn the_credential_posts_the_assertion_to_the_token_endpoint() {
        let (base, handle) = super::super::testing::serve(vec![
            (
                200,
                "application/json",
                r#"{"authorization_endpoint":"{BASE}/t/authorize","token_endpoint":"{BASE}/t/oauth2/v2.0/token","issuer":"{BASE}/t/v2.0"}"#.to_owned(),
            ),
            token_reply(),
        ])
        .await;
        let credential = credential(Authority::local(&base), "t", false);

        let token = credential.token("https://storage.azure.com/.default").await.unwrap();
        let request = handle.await.unwrap().remove(1);

        assert_eq!(token.secret, "aad");
        assert!(request.starts_with("POST /t/oauth2/v2.0/token HTTP/1.1"));
        assert!(request.contains("grant_type=client_credentials"));
        assert!(
            request.contains("client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer")
        );
        assert!(request.contains("scope=https%3A%2F%2Fstorage.azure.com%2F.default+openid+offline_access+profile"));
    }
}
