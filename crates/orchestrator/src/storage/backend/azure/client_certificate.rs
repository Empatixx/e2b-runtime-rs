use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p12_keystore::{KeyStore, Pkcs12ImportPolicy};
use reqwest::Client;
use ring::digest::{SHA256, digest};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PSS_SHA256, RsaKeyPair};

use super::super::pem::{self, PemBlock};
use super::aad::Authority;
use super::token_source::{BearerToken, SourceError, TokenSource};

const CREDENTIAL: &str = "ClientCertificateCredential";
const ANCHOR: &str = "client-cert";
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";
const ASSERTION_LIFETIME_SECONDS: u64 = 600;

pub struct ClientCertificate {
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
        let chain: Vec<Vec<u8>> = blocks
            .iter()
            .filter(|block| block.label == "CERTIFICATE")
            .map(|block| block.der.clone())
            .collect();
        let mut keys = blocks
            .iter()
            .filter(|block| block.label == "PRIVATE KEY" || block.label == "RSA PRIVATE KEY");
        let key = keys.next();
        if keys.next().is_some() {
            return Err("certData contains multiple private keys".to_owned());
        }
        if chain.is_empty() {
            return Err("found no certificate".to_owned());
        }
        let key = key.ok_or_else(|| "found no private key".to_owned())?;
        RsaKeyPair::from_pkcs8(&key.der)
            .or_else(|_| RsaKeyPair::from_der(&key.der))
            .map(|key| Self { chain, key })
            .map_err(|e| e.to_string())
    }

    pub fn assertion(
        &self,
        audience: &str,
        client_id: &str,
        send_chain: bool,
        now_unix: u64,
    ) -> Result<String, String> {
        let leaf = self.chain.first().ok_or_else(|| "found no certificate".to_owned())?;
        let thumbprint = STANDARD.encode(digest(&SHA256, leaf));
        let chain = match send_chain {
            true => format!(
                r#""x5c":[{}],"#,
                self.chain
                    .iter()
                    .map(|cert| format!("\"{}\"", STANDARD.encode(cert)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            false => String::new(),
        };
        let header = format!(r#"{{"alg":"PS256","typ":"JWT",{chain}"x5t#S256":"{thumbprint}"}}"#);
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
                &RSA_PSS_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|e| format!("unable to sign JWT token: {e}"))?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let assertion = self
            .certificate
            .assertion(self.authority.token_endpoint(), &self.client_id, self.send_chain, now)
            .map_err(|e| SourceError::Failed(format!("{CREDENTIAL}: {e}")))?;
        self.authority
            .request_token(
                &self.client,
                CREDENTIAL,
                ANCHOR,
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
    use ring::signature::{RSA_PSS_2048_8192_SHA256, UnparsedPublicKey};
    use tokio::io::AsyncWriteExt;

    use super::*;

    const PEM: &[u8] = include_bytes!("client_certificate/cert.pem");
    const PKCS12: &[u8] = include_bytes!("client_certificate/cert.p12");

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
            .assertion("https://login/t/oauth2/v2.0/token", "client", true, 1_700_000_000)
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
        let without_chain = certificate.assertion("a", "c", false, 1).unwrap();
        assert!(
            decoded(without_chain.split('.').next().unwrap()).starts_with(r#"{"alg":"PS256","typ":"JWT","x5t#S256""#)
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
            authority: Authority::with_token_endpoint(format!("http://{address}/t/oauth2/v2.0/token")),
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
