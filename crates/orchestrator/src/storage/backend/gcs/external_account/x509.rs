use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use tokio_rustls::rustls::client::ResolvesClientCert;
use tokio_rustls::rustls::crypto::ring::{default_provider, sign::any_supported_type};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, SignatureScheme};

use super::config::CredentialSource;
use crate::storage::backend::pem;

const NO_CONFIG: &str = "credentials: \"certificate\" object must either specify a certificate_config_location or use_default_certificate_config should be true";
const BOTH_CONFIG: &str = "credentials: \"certificate\" object cannot specify both a certificate_config_location and use_default_certificate_config=true";
const CONFIG_ENV: &str = "GOOGLE_API_CERTIFICATE_CONFIG";
const DEFAULT_CONFIG: &str = ".config/gcloud/certificate_config.json";
const SOURCE_UNAVAILABLE: &str = "certificate source is unavailable";
const ECP_UNSUPPORTED: &str = "enterprise certificate proxy configurations are not supported";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct X509Subject {
    trust_chain_path: String,
    config_path: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CertificateConfigFile {
    cert_configs: CertConfigs,
    libs: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CertConfigs {
    workload: Option<WorkloadConfig>,
    pkcs11: Option<serde_json::Value>,
    windows_store: Option<serde_json::Value>,
    macos_keychain: Option<serde_json::Value>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
struct WorkloadConfig {
    cert_path: String,
    key_path: String,
}

#[derive(Debug)]
struct WorkloadCertResolver {
    cert_path: String,
    key_path: String,
}

impl WorkloadCertResolver {
    fn load(&self) -> Result<CertifiedKey, String> {
        let read = |path: &str| {
            std::fs::read_to_string(path).map_err(|e| crate::go_os::path_error("open", Path::new(path), &e))
        };
        let certificates: Vec<CertificateDer<'static>> = pem::blocks(&read(&self.cert_path)?)
            .into_iter()
            .filter(|block| block.label == "CERTIFICATE")
            .map(|block| CertificateDer::from(block.der))
            .collect();
        if certificates.is_empty() {
            return Err("tls: failed to find any PEM data in certificate input".to_owned());
        }
        let key = pem::blocks(&read(&self.key_path)?)
            .into_iter()
            .find_map(|block| match block.label.as_str() {
                "PRIVATE KEY" => Some(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(block.der))),
                "RSA PRIVATE KEY" => Some(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(block.der))),
                "EC PRIVATE KEY" => Some(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(block.der))),
                _ => None,
            })
            .ok_or_else(|| "tls: failed to find any PEM data in key input".to_owned())?;
        any_supported_type(&key)
            .map(|signing_key| CertifiedKey::new(certificates, signing_key))
            .map_err(|e| format!("tls: failed to parse private key: {e}"))
    }
}

impl ResolvesClientCert for WorkloadCertResolver {
    fn resolve(&self, _root_hint_subjects: &[&[u8]], _sigschemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        self.load().ok().map(Arc::new)
    }

    fn has_certs(&self) -> bool {
        true
    }
}

impl X509Subject {
    pub fn new(source: &CredentialSource) -> Result<Self, String> {
        let certificate = source.certificate.clone().unwrap_or_default();
        match (
            certificate.use_default_certificate_config,
            certificate.certificate_config_location.is_empty(),
        ) {
            (false, true) => Err(NO_CONFIG.to_owned()),
            (true, false) => Err(BOTH_CONFIG.to_owned()),
            _ => Ok(Self {
                trust_chain_path: certificate.trust_chain_path,
                config_path: certificate.certificate_config_location,
            }),
        }
    }

    pub fn token(&self) -> Result<String, String> {
        let leaf = self
            .leaf()
            .map_err(|e| format!("credentials: failed to load leaf certificate: {e}"))?;
        let trust =
            trust_chain(&self.trust_chain_path).map_err(|e| format!("credentials: failed to read trust chain: {e}"))?;
        let chain = assemble(leaf, trust)?;
        serde_json::to_string(&chain.iter().map(|cert| STANDARD.encode(cert)).collect::<Vec<_>>())
            .map_err(|e| format!("credentials: failed to format certificate data: {e}"))
    }

    fn config_file(&self) -> String {
        match self.config_path.is_empty() {
            false => self.config_path.clone(),
            true => std::env::var(CONFIG_ENV)
                .ok()
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| format!("{}/{DEFAULT_CONFIG}", home_directory())),
        }
    }

    pub fn client(&self) -> Result<reqwest::Client, String> {
        let workload = self.workload()?;
        let resolver = WorkloadCertResolver {
            cert_path: workload.cert_path,
            key_path: workload.key_path,
        };
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_client_cert_resolver(Arc::new(resolver));
        reqwest::Client::builder()
            .use_preconfigured_tls(config)
            .timeout(CLIENT_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())
    }

    fn workload(&self) -> Result<WorkloadConfig, String> {
        let config_path = self.config_file();
        let bytes = std::fs::read(&config_path).map_err(|_| SOURCE_UNAVAILABLE.to_owned())?;
        let config: CertificateConfigFile = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        let Some(workload) = config.cert_configs.workload else {
            return match is_ecp(&config) {
                true => Err(ECP_UNSUPPORTED.to_owned()),
                false => Err(SOURCE_UNAVAILABLE.to_owned()),
            };
        };
        match (workload.cert_path.is_empty(), workload.key_path.is_empty()) {
            (true, _) => Err("certificate configuration is missing the certificate file location".to_owned()),
            (_, true) => Err("certificate configuration is missing the key file location".to_owned()),
            _ => Ok(workload),
        }
    }

    fn leaf(&self) -> Result<Vec<u8>, String> {
        let path = self
            .workload()
            .map_err(|e| format!("credentials: failed to get certificate path from config file: {e}"))?
            .cert_path;
        let data = std::fs::read(&path).map_err(|e| {
            format!(
                "credentials: failed to read leaf certificate file: {}",
                crate::go_os::path_error("open", Path::new(&path), &e)
            )
        })?;
        single_certificate(&data)
    }
}

fn is_ecp(config: &CertificateConfigFile) -> bool {
    let sections = [
        &config.cert_configs.pkcs11,
        &config.cert_configs.windows_store,
        &config.cert_configs.macos_keychain,
    ];
    sections.iter().any(|section| section.is_some()) && config.libs.is_some()
}

fn home_directory() -> String {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .or_else(crate::storage::backend::gcs::adc::current_user_home)
        .unwrap_or_default()
}

fn single_certificate(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("credentials: invalid certificate data: empty input".to_owned());
    }
    let blocks = pem::blocks(&String::from_utf8_lossy(data));
    let block = blocks
        .into_iter()
        .next()
        .ok_or_else(|| "credentials: invalid PEM-encoded certificate data: no PEM block found".to_owned())?;
    match block.label.as_str() {
        "CERTIFICATE" => Ok(block.der),
        other => Err(format!(
            "credentials: invalid PEM-encoded certificate data: expected CERTIFICATE block type, got {other}"
        )),
    }
}

fn trust_chain(path: &str) -> Result<Vec<Vec<u8>>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let data = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "credentials: trust chain file not found: {}",
            crate::go_os::path_error("open", Path::new(path), &e)
        ),
        _ => format!(
            "credentials: failed to read trust chain file: {}",
            crate::go_os::path_error("open", Path::new(path), &e)
        ),
    })?;
    Ok(pem::blocks(&String::from_utf8_lossy(&data))
        .into_iter()
        .filter(|block| block.label == "CERTIFICATE")
        .map(|block| block.der)
        .collect())
}

fn assemble(leaf: Vec<u8>, trust: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, String> {
    let mut chain = vec![leaf];
    let mut rest = trust.into_iter();
    if let Some(first) = rest.next().filter(|first| *first != chain[0]) {
        chain.push(first);
    }
    for cert in rest {
        if cert == chain[0] {
            return Err("credentials: the leaf certificate must be at the top of the trust chain file".to_owned());
        }
        chain.push(cert);
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_starts_with_the_leaf_and_rejects_a_misplaced_leaf() {
        let (leaf, ca, root) = (vec![1], vec![2], vec![3]);

        assert_eq!(
            assemble(leaf.clone(), vec![leaf.clone(), ca.clone(), root.clone()]).unwrap(),
            vec![leaf.clone(), ca.clone(), root.clone()]
        );
        assert_eq!(
            assemble(leaf.clone(), vec![ca.clone()]).unwrap(),
            vec![leaf.clone(), ca.clone()]
        );
        assert_eq!(
            assemble(leaf.clone(), vec![ca, leaf]).unwrap_err(),
            "credentials: the leaf certificate must be at the top of the trust chain file"
        );
    }

    #[test]
    fn the_certificate_source_needs_exactly_one_config_choice() {
        let source = |json: &str| {
            super::super::config::ExternalAccountFile::parse(
                format!(r#"{{"credential_source":{{"certificate":{json}}}}}"#).as_bytes(),
            )
            .unwrap()
            .credential_source
            .unwrap()
        };

        assert_eq!(X509Subject::new(&source("{}")).err().unwrap(), NO_CONFIG);
        assert_eq!(
            X509Subject::new(&source(
                r#"{"use_default_certificate_config":true,"certificate_config_location":"/c"}"#
            ))
            .err()
            .unwrap(),
            BOTH_CONFIG
        );
        assert!(X509Subject::new(&source(r#"{"certificate_config_location":"/c"}"#)).is_ok());
    }

    #[test]
    fn the_subject_token_is_a_json_array_of_base64_certificates() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("leaf.pem");
        std::fs::write(&cert, "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n").unwrap();
        let config = dir.path().join("config.json");
        std::fs::write(
            &config,
            format!(
                r#"{{"cert_configs":{{"workload":{{"cert_path":"{}","key_path":"k"}}}}}}"#,
                cert.display()
            ),
        )
        .unwrap();
        let subject = X509Subject {
            trust_chain_path: String::new(),
            config_path: config.to_str().unwrap().to_owned(),
        };

        assert_eq!(subject.token().unwrap(), r#"["AQID"]"#);
        assert_eq!(
            single_certificate(b"").unwrap_err(),
            "credentials: invalid certificate data: empty input"
        );
    }
}
