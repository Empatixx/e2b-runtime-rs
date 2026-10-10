use std::sync::{Arc, LazyLock};

use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::crypto::{CryptoProvider, ring};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme};

pub fn connector(insecure_skip_verify: bool) -> TlsConnector {
    let builder = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default protocol versions");
    let config = match insecure_skip_verify {
        true => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate::new()))
            .with_no_client_auth(),
        false => builder.with_root_certificates(web_roots()).with_no_client_auth(),
    };
    TlsConnector::from(Arc::new(config))
}

fn web_roots() -> RootCertStore {
    static SYSTEM_ROOTS: LazyLock<RootCertStore> = LazyLock::new(|| {
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        roots
    });
    SYSTEM_ROOTS.clone()
}

#[derive(Debug)]
struct AcceptAnyCertificate {
    provider: CryptoProvider,
}

impl AcceptAnyCertificate {
    fn new() -> Self {
        Self {
            provider: ring::default_provider(),
        }
    }
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
