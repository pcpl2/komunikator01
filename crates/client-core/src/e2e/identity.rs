use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct IdentityPublic {
    pub sign: [u8; 32],
    pub dh: [u8; 32],
    pub device: [u8; 16],
}

const FORMAT_V1: u8 = 1;
const NOISE_PARAMS: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";

/// Long-term device identity: Ed25519 signing key plus an X25519 key pair for Noise.
pub struct Identity {
    sign: SigningKey,
    dh_secret: [u8; 32],
    public: IdentityPublic,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS randomness unavailable");
    b
}

impl Identity {
    pub fn generate() -> Identity {
        let sign = SigningKey::from_bytes(&random::<32>());
        let kp = snow::Builder::new(NOISE_PARAMS.parse().expect("valid noise params"))
            .generate_keypair()
            .expect("x25519 keypair");
        let dh_secret: [u8; 32] = kp.private.try_into().expect("32-byte x25519 secret");
        let dh: [u8; 32] = kp.public.try_into().expect("32-byte x25519 public");
        let public = IdentityPublic {
            sign: sign.verifying_key().to_bytes(),
            dh,
            device: random::<16>(),
        };
        Identity { sign, dh_secret, public }
    }

    /// Versioned encoding: `1 || ed25519 secret || x25519 secret || x25519 public || device`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1 + 32 * 3 + 16);
        v.push(FORMAT_V1);
        v.extend_from_slice(&self.sign.to_bytes());
        v.extend_from_slice(&self.dh_secret);
        v.extend_from_slice(&self.public.dh);
        v.extend_from_slice(&self.public.device);
        v
    }

    pub fn from_bytes(b: &[u8]) -> Option<Identity> {
        if b.len() != 1 + 32 * 3 + 16 || b[0] != FORMAT_V1 {
            return None;
        }
        let sign = SigningKey::from_bytes(b[1..33].try_into().ok()?);
        let dh_secret: [u8; 32] = b[33..65].try_into().ok()?;
        let public = IdentityPublic {
            sign: sign.verifying_key().to_bytes(),
            dh: b[65..97].try_into().ok()?,
            device: b[97..113].try_into().ok()?,
        };
        Some(Identity { sign, dh_secret, public })
    }

    pub fn public(&self) -> IdentityPublic {
        self.public
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.sign.sign(msg).to_bytes()
    }

    pub fn dh_secret(&self) -> &[u8; 32] {
        &self.dh_secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_bytes_roundtrip() {
        let id = Identity::generate();
        let back = Identity::from_bytes(&id.to_bytes()).unwrap();
        assert_eq!(back.public(), id.public());
        assert!(Identity::from_bytes(&[9; 10]).is_none());
        let mut bad = id.to_bytes();
        bad[0] = 2;
        assert!(Identity::from_bytes(&bad).is_none());
    }

    #[test]
    fn debug_hides_secrets() {
        let id = Identity::generate();
        let s = format!("{id:?}");
        assert!(!s.contains(&format!("{:?}", id.dh_secret())));
    }
}
