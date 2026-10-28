use super::identity::{Identity, IdentityPublic};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

pub const ANNOUNCE_MAX_SKEW_MS: u64 = 3_600_000;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Announce {
    pub username: String,
    pub room: u32,
    pub identity: IdentityPublic,
    pub timestamp_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AnnounceError {
    Malformed,
    BadSignature,
    NameMismatch,
    WrongRoom,
    Stale,
}

#[derive(Serialize, Deserialize)]
struct SignedAnnounce {
    body: Vec<u8>,
    #[serde(with = "serde_big_array_64")]
    sig: [u8; 64],
}

/// serde only derives arrays up to 32; encode the 64-byte signature as a tuple.
mod serde_big_array_64 {
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::ser::SerializeTuple;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 64], s: S) -> Result<S::Ok, S::Error> {
        let mut t = s.serialize_tuple(64)?;
        for b in v {
            t.serialize_element(b)?;
        }
        t.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 64], D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = [u8; 64];
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("64 bytes")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; 64], A::Error> {
                let mut out = [0u8; 64];
                for (i, o) in out.iter_mut().enumerate() {
                    *o = seq.next_element()?.ok_or_else(|| A::Error::invalid_length(i, &self))?;
                }
                Ok(out)
            }
        }
        d.deserialize_tuple(64, V)
    }
}

const DOMAIN: &[u8] = b"komunikator/announce/v1";

fn signed_message(body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(DOMAIN.len() + body.len());
    m.extend_from_slice(DOMAIN);
    m.extend_from_slice(body);
    m
}

pub fn sign_announce(id: &Identity, username: &str, room: u32, now_ms: u64) -> Vec<u8> {
    let announce = Announce {
        username: username.to_owned(),
        room,
        identity: id.public(),
        timestamp_ms: now_ms,
    };
    let body = postcard::to_allocvec(&announce).expect("announce serializes");
    let sig = id.sign(&signed_message(&body));
    postcard::to_allocvec(&SignedAnnounce { body, sig }).expect("signed announce serializes")
}

pub fn verify_announce(
    blob: &[u8],
    expected_name: &str,
    room: u32,
    now_ms: u64,
) -> Result<Announce, AnnounceError> {
    let signed: SignedAnnounce = postcard::from_bytes(blob).map_err(|_| AnnounceError::Malformed)?;
    let announce: Announce =
        postcard::from_bytes(&signed.body).map_err(|_| AnnounceError::Malformed)?;
    let key = VerifyingKey::from_bytes(&announce.identity.sign)
        .map_err(|_| AnnounceError::BadSignature)?;
    key.verify_strict(&signed_message(&signed.body), &Signature::from_bytes(&signed.sig))
        .map_err(|_| AnnounceError::BadSignature)?;
    if announce.username != expected_name {
        return Err(AnnounceError::NameMismatch);
    }
    if announce.room != room {
        return Err(AnnounceError::WrongRoom);
    }
    if announce.timestamp_ms.abs_diff(now_ms) > ANNOUNCE_MAX_SKEW_MS {
        return Err(AnnounceError::Stale);
    }
    Ok(announce)
}

#[cfg(test)]
mod tests {
    use super::*;
    const T: u64 = 1_800_000_000_000;

    #[test]
    fn announce_roundtrip() {
        let id = Identity::generate();
        let blob = sign_announce(&id, "ala", 0, T);
        let a = verify_announce(&blob, "ala", 0, T).unwrap();
        assert_eq!(a.identity, id.public());
        assert_eq!(a.username, "ala");
    }

    #[test]
    fn forged_announce_rejected() {
        let id = Identity::generate();
        let blob = sign_announce(&id, "ala", 0, T);
        // Flip a byte inside the body (just after the length prefix).
        let mut tampered = blob.clone();
        tampered[2] ^= 1;
        assert_eq!(
            verify_announce(&tampered, "ala", 0, T),
            Err(AnnounceError::BadSignature)
        );
        // Valid signature from another identity over the same body.
        let other = Identity::generate();
        let mut sa: super::SignedAnnounce = postcard::from_bytes(&blob).unwrap();
        let mut msg = b"komunikator/announce/v1".to_vec();
        msg.extend_from_slice(&sa.body);
        sa.sig = other.sign(&msg);
        let forged = postcard::to_allocvec(&sa).unwrap();
        assert_eq!(
            verify_announce(&forged, "ala", 0, T),
            Err(AnnounceError::BadSignature)
        );
    }

    #[test]
    fn malformed_rejected() {
        assert_eq!(verify_announce(&[1, 2, 3], "ala", 0, T), Err(AnnounceError::Malformed));
    }

    #[test]
    fn name_mismatch_rejected() {
        let id = Identity::generate();
        let blob = sign_announce(&id, "ala", 0, T);
        assert_eq!(verify_announce(&blob, "ola", 0, T), Err(AnnounceError::NameMismatch));
    }

    #[test]
    fn wrong_room_rejected() {
        let id = Identity::generate();
        let blob = sign_announce(&id, "ala", 1, T);
        assert_eq!(verify_announce(&blob, "ala", 0, T), Err(AnnounceError::WrongRoom));
    }

    #[test]
    fn announce_tolerates_clock_skew() {
        let id = Identity::generate();
        let blob = sign_announce(&id, "ala", 0, T);
        let ten = 600_000;
        assert!(verify_announce(&blob, "ala", 0, T + ten).is_ok());
        assert!(verify_announce(&blob, "ala", 0, T - ten).is_ok());
        assert_eq!(
            verify_announce(&blob, "ala", 0, T + 7_200_000),
            Err(AnnounceError::Stale)
        );
        assert_eq!(
            verify_announce(&blob, "ala", 0, T - 7_200_000),
            Err(AnnounceError::Stale)
        );
    }
}
