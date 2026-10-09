use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p12_keystore::{KeyStore, Pkcs12ImportPolicy};
use reqwest::Client;
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RSA_PSS_SHA256, RsaEncoding, RsaKeyPair};

use super::super::pem::{self, PemBlock};
use super::aad::Authority;
use super::der;
use super::token_source::{BearerToken, SourceError, TokenSource};

const CREDENTIAL: &str = "ClientCertificateCredential";
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const ASSERTION_LIFETIME_SECONDS: u64 = 600;

pub struct ClientCertificate {
    signing: Vec<u8>,
    chain: Vec<Vec<u8>>,
    key: RsaKeyPair,
}

pub struct ClientCertificateCredential {
    pub client_id: String,
    pub certificate: ClientCertificate,
    pub send_chain: bool,
    pub authority: Authority,
    pub client: Client,
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

    fn algorithm(self) -> &'static str {
        match self {
            Self::Pss256 => "PS256",
            Self::Rs256 => "RS256",
        }
    }

    fn thumbprint_header(self) -> &'static str {
        match self {
            Self::Pss256 => "x5t#S256",
            Self::Rs256 => "x5t",
        }
    }

    fn thumbprint(self, certificate: &[u8]) -> String {
        let algorithm = match self {
            Self::Pss256 => &SHA256,
            Self::Rs256 => &SHA1_FOR_LEGACY_USE_ONLY,
        };
        STANDARD.encode(digest(algorithm, certificate))
    }

    fn padding(self) -> &'static dyn RsaEncoding {
        match self {
            Self::Pss256 => &RSA_PSS_SHA256,
            Self::Rs256 => &RSA_PKCS1_SHA256,
        }
    }
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
        let pair = rsa_key_pair(key)?;
        Self::matching(&certificates, pair)
    }

    fn matching(certificates: &[&[u8]], key: RsaKeyPair) -> Result<Self, String> {
        let public = key.public().as_ref().to_vec();
        let matches = |certificate: &&[u8]| der::certificate_rsa_public_key(certificate) == Some(public.as_slice());
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

    fn chain_header(&self, send_chain: bool) -> String {
        match send_chain {
            true => format!(
                r#""x5c":[{}],"#,
                self.chain
                    .iter()
                    .map(|certificate| format!("\"{}\"", STANDARD.encode(certificate)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            false => String::new(),
        }
    }

    pub fn assertion(
        &self,
        audience: &str,
        client_id: &str,
        send_chain: bool,
        now_unix: u64,
        signing: Signing,
    ) -> Result<String, String> {
        let header = format!(
            r#"{{"alg":"{}","typ":"JWT",{}"{}":"{}"}}"#,
            signing.algorithm(),
            self.chain_header(send_chain),
            signing.thumbprint_header(),
            signing.thumbprint(&self.signing)
        );
        let claims = format!(
            r#"{{"aud":{},"exp":{},"iss":{},"jti":"{}","nbf":{now_unix},"sub":{}}}"#,
            json_string(audience),
            now_unix + ASSERTION_LIFETIME_SECONDS,
            json_string(client_id),
            uuid::Uuid::new_v4(),
            json_string(client_id)
        );
        let signing_input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(claims));
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                signing.padding(),
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|e| format!("unable to sign JWT token: {e}"))?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

fn rsa_key_pair(key: &PemBlock) -> Result<RsaKeyPair, String> {
    if key.label == "PRIVATE KEY" && !der::pkcs8_is_rsa(&key.der) && RsaKeyPair::from_der(&key.der).is_err() {
        return Err("key must be an RSA key".to_owned());
    }
    RsaKeyPair::from_pkcs8(&key.der)
        .or_else(|_| RsaKeyPair::from_der(&key.der))
        .map_err(|e| e.to_string())
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
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

#[async_trait::async_trait]
impl TokenSource for ClientCertificateCredential {
    async fn token(&self, scope: &str) -> Result<BearerToken, SourceError> {
        let authority = self
            .authority
            .resolved(&self.client)
            .await
            .map_err(|e| e.failed(CREDENTIAL, |message| message))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let assertion = self
            .certificate
            .assertion(
                authority.token_endpoint(),
                &self.client_id,
                self.send_chain,
                now,
                Signing::of(&authority),
            )
            .map_err(|e| SourceError::Failed(format!("{CREDENTIAL}: {e}")))?;
        authority
            .request_token(
                &self.client,
                CREDENTIAL,
                scope,
                vec![
                    ("grant_type", "client_credentials".to_owned()),
                    ("client_assertion_type", ASSERTION_TYPE.to_owned()),
                    ("client_assertion", assertion),
                    ("client_id", self.client_id.clone()),
                    ("client_info", "1".to_owned()),
                ],
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RSA_PSS_2048_8192_SHA256, UnparsedPublicKey};
    use tokio::io::AsyncWriteExt;

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
        assert!(header.starts_with(r#"{"alg":"PS256","typ":"JWT","x5c":[""#));
        let thumbprint = STANDARD.encode(digest(&SHA256, &certificate.chain[0]));
        assert!(header.ends_with(&format!(r#""x5t#S256":"{thumbprint}"}}"#)));
        let claims = decoded(parts[1]);
        assert!(
            claims.starts_with(r#"{"aud":"https://login/t/oauth2/v2.0/token","exp":1700000600,"iss":"client","jti":""#)
        );
        assert!(claims.ends_with(r#"","nbf":1700000000,"sub":"client"}"#));
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PSS_2048_8192_SHA256, certificate.key.public().as_ref())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
        let without_chain = certificate.assertion("a", "c", false, 1, Signing::Pss256).unwrap();
        assert!(
            decoded(without_chain.split('.').next().unwrap()).starts_with(r#"{"alg":"PS256","typ":"JWT","x5t#S256""#)
        );
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
            format!(r#"{{"alg":"RS256","typ":"JWT","x5t":"{thumbprint}"}}"#)
        );
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, certificate.key.public().as_ref())
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();
    }

    #[tokio::test]
    async fn adfs_authorities_sign_for_the_discovered_endpoint() {
        let (base, handle) = super::super::testing::serve(vec![
            (
                200,
                "application/json",
                r#"{"authorization_endpoint":"{BASE}/adfs/oauth2/authorize","token_endpoint":"{BASE}/adfs/oauth2/token","issuer":"{BASE}/adfs"}"#.to_owned(),
            ),
            (200, "application/json", r#"{"access_token":"aad","expires_in":3600}"#.to_owned()),
        ])
        .await;
        let credential = ClientCertificateCredential {
            client_id: "client".to_owned(),
            certificate: ClientCertificate::parse(PEM, "").unwrap(),
            send_chain: false,
            authority: Authority::local_adfs(&base),
            client: Client::new(),
        };

        let token = credential.token("s").await.unwrap();
        let requests = handle.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(requests[1].starts_with("POST /adfs/oauth2/token HTTP/1.1"));
        let assertion = requests[1]
            .split("client_assertion=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .unwrap();
        assert!(decoded(assertion.split('.').next().unwrap()).starts_with(r#"{"alg":"RS256""#));
        assert!(
            decoded(assertion.split('.').nth(1).unwrap()).contains(&format!(r#""aud":"{base}/adfs/oauth2/token""#))
        );
    }

    #[tokio::test]
    async fn the_credential_posts_the_assertion_to_the_token_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = super::super::testing::read_request(&mut connection).await;
            let body = r#"{"access_token":"aad","expires_in":3600}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            connection.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        let credential = ClientCertificateCredential {
            client_id: "client".to_owned(),
            certificate: ClientCertificate::parse(PEM, "").unwrap(),
            send_chain: false,
            authority: Authority::local(&format!("http://{address}")),
            client: Client::new(),
        };

        let token = credential.token("https://storage.azure.com/.default").await.unwrap();
        let request = server.await.unwrap();

        assert_eq!(token.secret, "aad");
        assert!(request.starts_with("POST /t/oauth2/v2.0/token HTTP/1.1"));
        assert!(request.contains("grant_type=client_credentials"));
        assert!(
            request.contains("client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer")
        );
        assert!(request.contains("scope=https%3A%2F%2Fstorage.azure.com%2F.default+openid+offline_access+profile"));
    }
}
