//! Signed release manifest. The release key stays offline; the server only hosts files, so a
//! client trusts nothing it downloads until this signature and the file hash check out.

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Frozen forever: the update channel must work with any protocol version.
pub const UPDATE_ALPN: &[u8] = b"komunikator-update/1";
pub const MAX_MANIFEST: usize = 4096;
pub const MAX_RELEASE_SIZE: u64 = 64 * 1024 * 1024;
/// Domain separation so a release signature can never be replayed as another kind of signature.
const DOMAIN: &[u8] = b"komunikator/release/v1";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl Version {
    /// Parses exactly `major.minor.patch`, digits only.
    pub fn parse(s: &str) -> Option<Version> {
        let mut parts = s.split('.');
        let mut next = || {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse::<u16>().ok()
        };
        let v = Version { major: next()?, minor: next()?, patch: next()? };
        parts.next().is_none().then_some(v)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    pub file: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

#[derive(Serialize, Deserialize)]
struct SignedManifest {
    body: Vec<u8>,
    /// Two halves because serde has no impl for `[u8; 64]`; on the wire this is the same 64 raw bytes.
    signature: ([u8; 32], [u8; 32]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("malformed manifest")]
    Malformed,
    #[error("bad manifest signature")]
    BadSignature,
    #[error("bad file name in manifest")]
    BadFileName,
}

/// 1-64 chars of `[A-Za-z0-9._-]`, not starting with `.`: never a path.
pub fn plain_file_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn signed_message(body: &[u8]) -> Vec<u8> {
    [DOMAIN, body].concat()
}

pub fn sign_manifest(key: &SigningKey, m: &Manifest) -> Vec<u8> {
    let body = postcard::to_allocvec(m).expect("manifest serializes");
    let sig = key.sign(&signed_message(&body)).to_bytes();
    let signature = (sig[..32].try_into().expect("32 bytes"), sig[32..].try_into().expect("32 bytes"));
    postcard::to_allocvec(&SignedManifest { body, signature }).expect("signed manifest serializes")
}

pub fn verify_manifest(public: &[u8; 32], bytes: &[u8]) -> Result<Manifest, UpdateError> {
    if bytes.len() > MAX_MANIFEST {
        return Err(UpdateError::Malformed);
    }
    let (signed, rest) =
        postcard::take_from_bytes::<SignedManifest>(bytes).map_err(|_| UpdateError::Malformed)?;
    if !rest.is_empty() {
        return Err(UpdateError::Malformed);
    }
    let key = VerifyingKey::from_bytes(public).map_err(|_| UpdateError::BadSignature)?;
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&signed.signature.0);
    sig[32..].copy_from_slice(&signed.signature.1);
    key.verify_strict(&signed_message(&signed.body), &Signature::from_bytes(&sig))
        .map_err(|_| UpdateError::BadSignature)?;
    let (m, rest) =
        postcard::take_from_bytes::<Manifest>(&signed.body).map_err(|_| UpdateError::Malformed)?;
    if !rest.is_empty() {
        return Err(UpdateError::Malformed);
    }
    if !plain_file_name(&m.file) {
        return Err(UpdateError::BadFileName);
    }
    Ok(m)
}

/// The manifest's `file` field with the signature unchecked. Only for the server, which has no
/// key and needs nothing but the name of the one file it may serve; clients use `verify_manifest`.
pub fn unverified_file_name(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_MANIFEST {
        return None;
    }
    let (signed, rest) = postcard::take_from_bytes::<SignedManifest>(bytes).ok()?;
    let (m, body_rest) = postcard::take_from_bytes::<Manifest>(&signed.body).ok()?;
    (rest.is_empty() && body_rest.is_empty()).then_some(m.file)
}

/// Strictly greater: equal or older is never an update (no downgrade).
pub fn is_newer(candidate: Version, current: Version) -> bool {
    candidate > current
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7; 32])
    }

    fn manifest(file: &str) -> Manifest {
        Manifest { version: Version { major: 1, minor: 2, patch: 3 }, file: file.into(), size: 1234, sha256: [9; 32] }
    }

    #[test]
    fn version_parse_and_order() {
        assert_eq!(Version::parse("1.2.3"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("a.b.c"), None);
        assert_eq!(Version::parse("1.2.-3"), None);
        assert_eq!(Version::parse("70000.0.0"), None);
        assert_eq!(Version::parse("1.2.3").unwrap().to_string(), "1.2.3");
        assert!(Version::parse("1.10.0").unwrap() > Version::parse("1.9.9").unwrap());
    }

    #[test]
    fn sign_verify_roundtrip() {
        let m = manifest("komunikator-1.2.3.exe");
        let bytes = sign_manifest(&key(), &m);
        assert_eq!(verify_manifest(&key().verifying_key().to_bytes(), &bytes), Ok(m));
    }

    #[test]
    fn tampered_body_or_signature_fails() {
        let public = key().verifying_key().to_bytes();
        let signed = sign_manifest(&key(), &manifest("a.exe"));
        for i in 0..signed.len() {
            let mut bad = signed.clone();
            bad[i] ^= 1;
            assert!(verify_manifest(&public, &bad).is_err(), "byte {i}");
        }
    }

    #[test]
    fn wrong_key_fails() {
        let other = SigningKey::from_bytes(&[8; 32]);
        let bytes = sign_manifest(&key(), &manifest("a.exe"));
        assert_eq!(verify_manifest(&other.verifying_key().to_bytes(), &bytes), Err(UpdateError::BadSignature));
    }

    #[test]
    fn manifest_file_name_is_plain() {
        let public = key().verifying_key().to_bytes();
        for bad in [r"..\evil.exe", "a/b", ".hidden", "", &"a".repeat(65)] {
            let bytes = sign_manifest(&key(), &manifest(bad));
            assert_eq!(verify_manifest(&public, &bytes), Err(UpdateError::BadFileName), "{bad:?}");
        }
        assert!(plain_file_name("komunikator-1.2.3.exe"));
        assert!(plain_file_name(&"a".repeat(64)));
    }

    #[test]
    fn older_or_equal_version_is_refused() {
        let v = |s| Version::parse(s).unwrap();
        assert!(is_newer(v("1.2.4"), v("1.2.3")));
        assert!(is_newer(v("2.0.0"), v("1.9.9")));
        assert!(!is_newer(v("1.2.3"), v("1.2.3")));
        assert!(!is_newer(v("1.2.2"), v("1.2.3")));
    }

    #[test]
    fn unverified_file_name_reads_without_key() {
        let bytes = sign_manifest(&key(), &manifest(r"..\x"));
        assert_eq!(unverified_file_name(&bytes).as_deref(), Some(r"..\x"));
        assert_eq!(unverified_file_name(&bytes[..bytes.len() - 1]), None);
        assert_eq!(unverified_file_name(&[bytes.clone(), vec![0]].concat()), None);
        assert_eq!(unverified_file_name(&[]), None);
    }

    #[test]
    fn oversized_manifest_is_malformed() {
        let public = key().verifying_key().to_bytes();
        let bytes = sign_manifest(&key(), &manifest("a.exe"));
        let mut big = bytes.clone();
        big.resize(MAX_MANIFEST + 1, 0);
        assert_eq!(verify_manifest(&public, &big), Err(UpdateError::Malformed));
        assert_eq!(verify_manifest(&public, &[]), Err(UpdateError::Malformed));
    }
}
