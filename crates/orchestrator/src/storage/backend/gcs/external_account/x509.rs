use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use super::config::CredentialSource;
use crate::storage::backend::pem;

const NO_CONFIG: &str = "credentials: \"certificate\" object must either specify a certificate_config_location or use_default_certificate_config should be true";
const BOTH_CONFIG: &str = "credentials: \"certificate\" object cannot specify both a certificate_config_location and use_default_certificate_config=true";
const DEFAULT_CONFIG_ENV: &str = "GOOGLE_API_CERTIFICATE_CONFIG";
const WELL_KNOWN_CONFIG: &str = ".secureConnect/context_aware_metadata.json";

pub struct X509Subject {
    trust_chain_path: String,
    config_path: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CertificateConfigFile {
    cert_configs: CertConfigs,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CertConfigs {
    workload: WorkloadConfig,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WorkloadConfig {
    cert_path: String,
    key_path: String,
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
            .map_err(|e| format!("failed to load leaf certificate: {e}"))?;
        let trust = trust_chain(&self.trust_chain_path).map_err(|e| format!("failed to read trust chain: {e}"))?;
        let chain = assemble(leaf, trust)?;
        serde_json::to_string(&chain.iter().map(|cert| STANDARD.encode(cert)).collect::<Vec<_>>())
            .map_err(|e| format!("failed to format certificate data: {e}"))
    }

    fn config_file(&self) -> String {
        match self.config_path.is_empty() {
            false => self.config_path.clone(),
            true => std::env::var(DEFAULT_CONFIG_ENV)
                .ok()
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| {
                    let home = std::env::var("HOME").unwrap_or_default();
                    format!("{home}/{WELL_KNOWN_CONFIG}")
                }),
        }
    }

    pub fn client(&self) -> Result<reqwest::Client, String> {
        let workload = self.workload()?;
        let read = |path: &str| std::fs::read(path).map_err(|e| crate::go_os::path_error("open", Path::new(path), &e));
        let mut pem = read(&workload.cert_path)?;
        pem.push(b'\n');
        pem.extend(read(&workload.key_path)?);
        reqwest::Identity::from_pem(&pem)
            .and_then(|identity| {
                reqwest::Client::builder()
                    .identity(identity)
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
            })
            .map_err(|e| e.to_string())
    }

    fn workload(&self) -> Result<WorkloadConfig, String> {
        let config_path = self.config_file();
        std::fs::read(&config_path)
            .map_err(|e| {
                format!(
                    "failed to get certificate path from config file: {}",
                    crate::go_os::path_error("open", Path::new(&config_path), &e)
                )
            })
            .and_then(|bytes| {
                serde_json::from_slice::<CertificateConfigFile>(&bytes)
                    .map_err(|e| format!("failed to get certificate path from config file: {e}"))
            })
            .map(|config| config.cert_configs.workload)
    }

    fn leaf(&self) -> Result<Vec<u8>, String> {
        let path = self.workload()?.cert_path;
        let data = std::fs::read(&path).map_err(|e| {
            format!(
                "failed to read leaf certificate file: {}",
                crate::go_os::path_error("open", Path::new(&path), &e)
            )
        })?;
        single_certificate(&data)
    }
}

fn single_certificate(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("invalid certificate data: empty input".to_owned());
    }
    let blocks = pem::blocks(&String::from_utf8_lossy(data));
    let block = blocks
        .into_iter()
        .next()
        .ok_or_else(|| "invalid PEM-encoded certificate data: no PEM block found".to_owned())?;
    match block.label.as_str() {
        "CERTIFICATE" => Ok(block.der),
        other => Err(format!(
            "invalid PEM-encoded certificate data: expected CERTIFICATE block type, got {other}"
        )),
    }
}

fn trust_chain(path: &str) -> Result<Vec<Vec<u8>>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let data = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "trust chain file not found: {}",
            crate::go_os::path_error("open", Path::new(path), &e)
        ),
        _ => format!(
            "failed to read trust chain file: {}",
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
            return Err("the leaf certificate must be at the top of the trust chain file".to_owned());
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
            "the leaf certificate must be at the top of the trust chain file"
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
            "invalid certificate data: empty input"
        );
    }
}
