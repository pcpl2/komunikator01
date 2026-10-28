//! Persistent self-signed server identity; clients pin its SHA-256 fingerprint.

use std::path::Path;

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

pub struct ServerIdentity {
    pub cert: CertificateDer<'static>,
    pub key: PrivatePkcs8KeyDer<'static>,
    pub fingerprint: [u8; 32],
}

/// Loads `cert.der`/`key.der` from `dir`, generating them on first run.
pub fn load_or_create_identity(dir: &Path) -> anyhow::Result<ServerIdentity> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    let (cert, key) = if cert_path.exists() && key_path.exists() {
        (
            std::fs::read(&cert_path).with_context(|| format!("reading {}", cert_path.display()))?,
            std::fs::read(&key_path).with_context(|| format!("reading {}", key_path.display()))?,
        )
    } else {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let generated = rcgen::generate_simple_self_signed(vec![protocol_server_name()])?;
        let cert = generated.cert.der().to_vec();
        let key = generated.signing_key.serialize_der();
        std::fs::write(&key_path, &key)?;
        std::fs::write(&cert_path, &cert)?;
        (cert, key)
    };
    let fingerprint = Sha256::digest(&cert).into();
    Ok(ServerIdentity {
        cert: CertificateDer::from(cert),
        key: PrivatePkcs8KeyDer::from(key),
        fingerprint,
    })
}

/// Secret for QUIC stateless resets. Persisting it lets a restarted (or crashed and
/// restarted) server end its old connections at once instead of after the idle timeout.
pub fn load_or_create_reset_key(dir: &Path) -> anyhow::Result<[u8; 32]> {
    let path = dir.join("reset.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(key);
        }
    }
    let mut key = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key)
        .map_err(|_| anyhow::anyhow!("system RNG failed"))?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, key).with_context(|| format!("writing {}", path.display()))?;
    Ok(key)
}

/// SAN embedded in the certificate; clients use it as the TLS server name.
fn protocol_server_name() -> String {
    "komunikator".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create_identity(dir.path()).unwrap();
        let b = load_or_create_identity(dir.path()).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.cert, b.cert);
    }

    #[test]
    fn reset_key_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create_reset_key(dir.path()).unwrap();
        assert_eq!(a, load_or_create_reset_key(dir.path()).unwrap());
        assert_ne!(a, [0; 32]);
    }
}
