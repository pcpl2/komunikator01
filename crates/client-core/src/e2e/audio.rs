//! Sender keys and authenticated encryption of audio frames.

use std::time::{Duration, Instant};

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use hkdf::Hkdf;
use protocol::audio::audio_aad;
use protocol::MemberId;
use sha2::Sha256;

pub const TAG_LEN: usize = 16;
const OLD_EPOCH_GRACE: Duration = Duration::from_secs(2);
const AUDIO_INFO: &[u8] = b"komunikator/audio/v1";

#[derive(Clone)]
pub struct SenderKey {
    pub epoch: u16,
    key: [u8; 32],
}

impl std::fmt::Debug for SenderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderKey").field("epoch", &self.epoch).finish_non_exhaustive()
    }
}

impl SenderKey {
    pub fn generate(epoch: u16) -> Self {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("system randomness unavailable");
        Self { epoch, key }
    }

    pub fn from_parts(epoch: u16, key: [u8; 32]) -> Self {
        Self { epoch, key }
    }

    pub fn secret(&self) -> &[u8; 32] {
        &self.key
    }

    pub fn audio_key(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        Hkdf::<Sha256>::new(None, &self.key)
            .expand(AUDIO_INFO, &mut out)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        out
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&self.audio_key().into())
    }
}

fn nonce(seq: u32) -> chacha20poly1305::Nonce {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&seq.to_le_bytes());
    n.into()
}

/// `a` is newer than `b` in wrapping u16 epoch order.
fn newer(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) > 0
}

pub struct AudioSealer {
    me: MemberId,
    epoch: u16,
    cipher: ChaCha20Poly1305,
    last_seq: Option<u32>,
}

impl AudioSealer {
    pub fn new(me: MemberId, key: &SenderKey) -> Self {
        Self { me, epoch: key.epoch, cipher: key.cipher(), last_seq: None }
    }

    /// Encrypts `opus` into `out` (ciphertext followed by tag). Returns the epoch, or `None`
    /// when `seq` does not exceed the last one used under this key (nonce reuse).
    pub fn seal(&mut self, seq: u32, opus: &[u8], out: &mut Vec<u8>) -> Option<u16> {
        if self.last_seq.is_some_and(|l| seq <= l) {
            return None;
        }
        out.clear();
        out.extend_from_slice(opus);
        let aad = audio_aad(self.me, self.epoch, seq);
        let tag = self.cipher.encrypt_in_place_detached(&nonce(seq), &aad, out).ok()?;
        out.extend_from_slice(&tag);
        self.last_seq = Some(seq);
        Some(self.epoch)
    }
}

/// Sliding 64-frame replay window relative to the highest sequence number seen.
#[derive(Default)]
struct Replay {
    seen: bool,
    max: u32,
    /// Bit `d` set means `max - d` was seen.
    bitmap: u64,
}

impl Replay {
    fn fresh(&self, seq: u32) -> bool {
        if !self.seen || seq > self.max {
            return true;
        }
        let d = self.max - seq;
        d < 64 && self.bitmap & (1u64 << d) == 0
    }

    fn mark(&mut self, seq: u32) {
        if !self.seen || seq > self.max {
            let shift = if self.seen { seq - self.max } else { 64 };
            self.bitmap = if shift >= 64 { 0 } else { self.bitmap << shift };
            self.bitmap |= 1;
            self.max = seq;
            self.seen = true;
        } else {
            self.bitmap |= 1u64 << (self.max - seq);
        }
    }
}

struct Slot {
    epoch: u16,
    cipher: ChaCha20Poly1305,
    replay: Replay,
    expires: Option<Instant>,
}

/// Decrypts frames of one sender. Keeps at most two keys: the newest added one and the one of
/// the epoch that most recently opened a valid frame.
pub struct AudioOpener {
    sender: MemberId,
    slots: Vec<Slot>,
    newest: Option<u16>,
    last_used: Option<u16>,
    highest_opened: Option<u16>,
}

impl AudioOpener {
    pub fn new(sender: MemberId) -> Self {
        Self { sender, slots: Vec::new(), newest: None, last_used: None, highest_opened: None }
    }

    pub fn add_key(&mut self, key: SenderKey) {
        // A peer must not swap the key of an epoch we already hold; an identical re-add is a
        // no-op that keeps the replay state.
        if self.slots.iter().any(|s| s.epoch == key.epoch) {
            return;
        }
        if self.newest.is_some_and(|n| !newer(key.epoch, n)) {
            return;
        }
        self.slots.push(Slot {
            epoch: key.epoch,
            cipher: key.cipher(),
            replay: Replay::default(),
            expires: None,
        });
        self.newest = Some(key.epoch);
        // The first key counts as in use until a frame says otherwise, so a key that has not
        // yet seen traffic survives the next rotation.
        self.last_used.get_or_insert(key.epoch);
        let (newest, last_used) = (self.newest, self.last_used);
        self.slots.retain(|s| Some(s.epoch) == newest || Some(s.epoch) == last_used);
    }

    pub fn open(
        &mut self,
        epoch: u16,
        seq: u32,
        ct: &[u8],
        now: Instant,
        out: &mut Vec<u8>,
    ) -> bool {
        self.slots.retain(|s| s.expires.is_none_or(|d| now < d));
        let Some(slot) = self.slots.iter_mut().find(|s| s.epoch == epoch) else {
            return false;
        };
        if ct.len() < TAG_LEN || !slot.replay.fresh(seq) {
            return false;
        }
        let (body, tag) = ct.split_at(ct.len() - TAG_LEN);
        out.clear();
        out.extend_from_slice(body);
        let aad = audio_aad(self.sender, epoch, seq);
        if slot.cipher.decrypt_in_place_detached(&nonce(seq), &aad, out, tag.into()).is_err() {
            out.clear();
            return false;
        }
        slot.replay.mark(seq);
        if self.highest_opened.is_none_or(|h| newer(epoch, h)) {
            self.highest_opened = Some(epoch);
            for s in self.slots.iter_mut().filter(|s| newer(epoch, s.epoch)) {
                s.expires.get_or_insert(now + OLD_EPOCH_GRACE);
            }
        }
        self.last_used = Some(epoch);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: MemberId = 7;

    fn pair(epoch: u16) -> (AudioSealer, AudioOpener, SenderKey) {
        let k = SenderKey::generate(epoch);
        let mut o = AudioOpener::new(S);
        o.add_key(k.clone());
        (AudioSealer::new(S, &k), o, k)
    }

    fn frame(k: &SenderKey, seq: u32) -> Vec<u8> {
        let mut ct = Vec::new();
        AudioSealer::new(S, k).seal(seq, b"f", &mut ct).unwrap();
        ct
    }

    #[test]
    fn seal_open_roundtrip() {
        let (mut s, mut o, _) = pair(1);
        let (mut ct, mut pt) = (Vec::new(), Vec::new());
        let e = s.seal(5, b"opus", &mut ct).unwrap();
        assert_eq!(ct.len(), 4 + TAG_LEN);
        assert!(o.open(e, 5, &ct, Instant::now(), &mut pt));
        assert_eq!(pt, b"opus");
    }

    #[test]
    fn tampered_header_fails() {
        let k = SenderKey::generate(1);
        let mut ct = Vec::new();
        AudioSealer::new(S, &k).seal(5, b"opus", &mut ct).unwrap();
        let mut pt = Vec::new();
        let now = Instant::now();
        let mut o = AudioOpener::new(S + 1);
        o.add_key(k.clone());
        assert!(!o.open(1, 5, &ct, now, &mut pt));
        let mut o = AudioOpener::new(S);
        o.add_key(k.clone());
        o.add_key(SenderKey::from_parts(2, *k.secret()));
        assert!(!o.open(2, 5, &ct, now, &mut pt));
        assert!(!o.open(1, 6, &ct, now, &mut pt));
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(!o.open(1, 5, &bad, now, &mut pt));
        assert!(o.open(1, 5, &ct, now, &mut pt), "failed opens must not burn the seq");
    }

    #[test]
    fn replay_rejected() {
        let (_, mut o, k) = pair(1);
        let mut pt = Vec::new();
        let now = Instant::now();
        let (f100, f90, f30) = (frame(&k, 100), frame(&k, 90), frame(&k, 30));
        assert!(o.open(1, 100, &f100, now, &mut pt));
        assert!(!o.open(1, 100, &f100, now, &mut pt));
        assert!(o.open(1, 90, &f90, now, &mut pt));
        assert!(!o.open(1, 90, &f90, now, &mut pt));
        assert!(!o.open(1, 30, &f30, now, &mut pt));
    }

    #[test]
    fn sealer_refuses_seq_reuse() {
        let k = SenderKey::generate(1);
        let mut s = AudioSealer::new(S, &k);
        let mut ct = Vec::new();
        assert!(s.seal(5, b"a", &mut ct).is_some());
        assert!(s.seal(5, b"a", &mut ct).is_none());
        assert!(s.seal(4, b"a", &mut ct).is_none());
        assert!(s.seal(6, b"a", &mut ct).is_some());
        assert!(AudioSealer::new(S, &k).seal(0, b"a", &mut ct).is_some());
    }

    #[test]
    fn previous_epoch_expires_after_switch() {
        let (k1, k2) = (SenderKey::generate(1), SenderKey::generate(2));
        let mut o = AudioOpener::new(S);
        o.add_key(k1.clone());
        o.add_key(k2.clone());
        let (t0, mut pt) = (Instant::now(), Vec::new());
        assert!(o.open(2, 1, &frame(&k2, 1), t0, &mut pt));
        assert!(o.open(1, 1, &frame(&k1, 1), t0 + Duration::from_secs(1), &mut pt));
        assert!(!o.open(1, 2, &frame(&k1, 2), t0 + Duration::from_secs(3), &mut pt));
    }

    #[test]
    fn churn_keeps_key_in_use() {
        let (k1, k2, k3) = (SenderKey::generate(1), SenderKey::generate(2), SenderKey::generate(3));
        let mut o = AudioOpener::new(S);
        let (t0, mut pt) = (Instant::now(), Vec::new());
        o.add_key(k1.clone());
        assert!(o.open(1, 1, &frame(&k1, 1), t0, &mut pt));
        o.add_key(k2);
        o.add_key(k3.clone());
        assert!(o.open(1, 2, &frame(&k1, 2), t0, &mut pt));
        assert!(o.open(3, 1, &frame(&k3, 1), t0, &mut pt));
        assert!(!o.open(1, 3, &frame(&k1, 3), t0 + Duration::from_secs(3), &mut pt));
    }

    #[test]
    fn epoch_key_cannot_be_swapped_or_rolled_back() {
        let mut o = AudioOpener::new(S);
        let (k1, evil) = (SenderKey::generate(1), SenderKey::generate(1));
        let (t0, mut pt) = (Instant::now(), Vec::new());
        o.add_key(k1.clone());
        o.add_key(evil.clone());
        assert!(!o.open(1, 1, &frame(&evil, 1), t0, &mut pt));
        assert!(o.open(1, 1, &frame(&k1, 1), t0, &mut pt));
        let k5 = SenderKey::generate(5);
        let k3 = SenderKey::generate(3);
        o.add_key(k5);
        o.add_key(k3.clone());
        assert!(!o.open(3, 1, &frame(&k3, 1), t0, &mut pt));
    }

    #[test]
    fn unknown_epoch_fails() {
        let (mut s, mut o, _) = pair(1);
        let (mut ct, mut pt) = (Vec::new(), Vec::new());
        s.seal(1, b"x", &mut ct).unwrap();
        assert!(!o.open(9, 1, &ct, Instant::now(), &mut pt));
    }

    #[test]
    fn audio_key_differs_from_secret() {
        let k = SenderKey::from_parts(1, [9; 32]);
        assert_ne!(&k.audio_key(), k.secret());
        assert_eq!(k.audio_key(), SenderKey::from_parts(1, [9; 32]).audio_key());
        assert!(!format!("{k:?}").contains("09"));
    }
}
