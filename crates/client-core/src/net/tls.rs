//! QUIC client configuration that trusts exactly one pinned server certificate.

use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

/// TLS server name; matches the SAN in the server's self-signed certificate.
pub const SERVER_NAME: &str = "komunikator";

pub fn client_config(fingerprint: [u8; 32]) -> anyhow::Result<quinn::ClientConfig> {
    client_config_with_alpn(fingerprint, protocol::ALPN)
}

/// The same pinned configuration speaking another protocol on the server's port, such as the
/// update channel (`protocol::update::UPDATE_ALPN`).
pub fn client_config_with_alpn(fingerprint: [u8; 32], alpn: &[u8]) -> anyhow::Result<quinn::ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier::new(fingerprint)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport_config()));
    Ok(config)
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_millis(protocol::KEEP_ALIVE_MS)));
    t.max_idle_timeout(Some(
        Duration::from_millis(protocol::IDLE_TIMEOUT_MS).try_into().expect("idle timeout fits"),
    ));
    t.datagram_receive_buffer_size(Some(64 * 1024));
    t
}

/// Accepts only the certificate whose SHA-256 equals the pinned fingerprint.
#[derive(Debug)]
pub(crate) struct PinnedVerifier {
    fingerprint: [u8; 32],
    provider: CryptoProvider,
}

impl PinnedVerifier {
    pub(crate) fn new(fingerprint: [u8; 32]) -> Self {
        Self { fingerprint, provider: ring::default_provider() }
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(end_entity).into();
        if actual == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("server certificate fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{ServerName, UnixTime};

    fn cert() -> CertificateDer<'static> {
        rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).unwrap().cert.der().clone()
    }

    #[test]
    fn verifier_accepts_matching_and_rejects_other() {
        let good = cert();
        let other = cert();
        let fp: [u8; 32] = Sha256::digest(&good).into();
        let v = PinnedVerifier::new(fp);
        let name = ServerName::try_from(SERVER_NAME).unwrap();
        assert!(v.verify_server_cert(&good, &[], &name, &[], UnixTime::now()).is_ok());
        assert!(v.verify_server_cert(&other, &[], &name, &[], UnixTime::now()).is_err());
    }

    #[test]
    fn client_config_builds() {
        assert!(client_config([0; 32]).is_ok());
    }
}
