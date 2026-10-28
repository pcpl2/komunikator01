//! Authenticated encryption of chat messages under the sender keys used for audio.

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit};
use protocol::{MemberId, RoomId};
use serde::{Deserialize, Serialize};

use super::audio::{SenderKey, TAG_LEN};
use crate::chat::MAX_CHAT_CHARS;

/// HKDF info of the chat key and prefix of the AAD.
pub(super) const CHAT_INFO: &[u8] = b"komunikator/chat/v1";
const AAD_LEN: usize = CHAT_INFO.len() + 4 + 2 + 2 + 4;
/// Epochs kept per sender: the current one plus two rotations of late messages.
const KEPT_EPOCHS: usize = 3;

/// Wire form of a chat message (the `blob` of `ClientMsg::Chat`).
#[derive(Serialize, Deserialize)]
struct SealedChat {
    epoch: u16,
    counter: u32,
    ct: Vec<u8>,
}

/// Binds a message to its room, sender, key and position, so the server cannot move it.
fn aad(room: RoomId, sender: MemberId, epoch: u16, counter: u32) -> [u8; AAD_LEN] {
    let mut a = [0u8; AAD_LEN];
    let (info, rest) = a.split_at_mut(CHAT_INFO.len());
    info.copy_from_slice(CHAT_INFO);
    rest[..4].copy_from_slice(&room.to_le_bytes());
    rest[4..6].copy_from_slice(&sender.to_le_bytes());
    rest[6..8].copy_from_slice(&epoch.to_le_bytes());
    rest[8..].copy_from_slice(&counter.to_le_bytes());
    a
}

fn nonce(counter: u32) -> chacha20poly1305::Nonce {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&counter.to_le_bytes());
    n.into()
}

fn cipher(key: &SenderKey) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&key.chat_key().into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatError {
    /// No pairwise link to the sender, so none of its keys.
    NoKey,
    Malformed,
    UnknownEpoch,
    Replay,
    BadTag,
    NotUtf8,
}

pub struct ChatSealer {
    epoch: u16,
    cipher: ChaCha20Poly1305,
    /// Next counter to use; `None` once `u32::MAX` was used (the key is spent).
    counter: Option<u32>,
}

impl ChatSealer {
    /// Invariant, as for `AudioSealer`: one sealer per `SenderKey`, or counters (nonces) repeat.
    pub fn new(key: &SenderKey) -> Self {
        Self { epoch: key.epoch, cipher: cipher(key), counter: Some(0) }
    }

    /// Encrypts `text` for `room`; `None` if it exceeds `MAX_CHAT_CHARS` or the key is spent.
    pub fn seal(&mut self, room: RoomId, me: MemberId, text: &str) -> Option<Vec<u8>> {
        if text.chars().count() > MAX_CHAT_CHARS {
            return None;
        }
        let counter = self.counter?;
        let mut ct = Vec::with_capacity(text.len() + TAG_LEN);
        ct.extend_from_slice(text.as_bytes());
        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce(counter), &aad(room, me, self.epoch, counter), &mut ct)
            .ok()?;
        ct.extend_from_slice(&tag);
        self.counter = counter.checked_add(1);
        postcard::to_allocvec(&SealedChat { epoch: self.epoch, counter, ct }).ok()
    }
}

struct Slot {
    epoch: u16,
    cipher: ChaCha20Poly1305,
    /// Highest counter opened under this key.
    last: Option<u32>,
}

/// Decrypts chat of one sender. Holds the `KEPT_EPOCHS` newest keys, so a message sealed just
/// before a rotation still opens after it.
///
/// Unlike `AudioOpener`, a key older than the newest is still accepted (and dropped if it is
/// not among the three newest). That is safe: keys arrive only over the sender's own Noise
/// channel, so only the sender can add them, and a known epoch's key is never replaced, so its
/// replay state cannot be reset.
#[derive(Default)]
pub struct ChatOpener {
    slots: Vec<Slot>,
}

impl ChatOpener {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_key(&mut self, key: &SenderKey) {
        // A peer must not swap the key of an epoch we already hold (it would reset the counter).
        if self.slots.iter().any(|s| s.epoch == key.epoch) {
            return;
        }
        self.slots.push(Slot { epoch: key.epoch, cipher: cipher(key), last: None });
        if self.slots.len() > KEPT_EPOCHS {
            // Ages relative to the newest epoch, in wrapping u16 order.
            let newest = self
                .slots
                .iter()
                .map(|s| s.epoch)
                .reduce(|a, b| if (b.wrapping_sub(a) as i16) > 0 { b } else { a })
                .unwrap_or(key.epoch);
            self.slots.sort_by_key(|s| newest.wrapping_sub(s.epoch));
            self.slots.truncate(KEPT_EPOCHS);
        }
    }

    /// The control stream is ordered, so per epoch only strictly increasing counters open.
    pub fn open(&mut self, room: RoomId, sender: MemberId, blob: &[u8]) -> Result<String, ChatError> {
        let (sealed, rest) =
            postcard::take_from_bytes::<SealedChat>(blob).map_err(|_| ChatError::Malformed)?;
        if !rest.is_empty() || sealed.ct.len() < TAG_LEN {
            return Err(ChatError::Malformed);
        }
        let slot = self.slots.iter_mut().find(|s| s.epoch == sealed.epoch).ok_or(ChatError::UnknownEpoch)?;
        if slot.last.is_some_and(|l| sealed.counter <= l) {
            return Err(ChatError::Replay);
        }
        let mut pt = sealed.ct;
        let tag = pt.split_off(pt.len() - TAG_LEN);
        let aad = aad(room, sender, sealed.epoch, sealed.counter);
        slot.cipher
            .decrypt_in_place_detached(&nonce(sealed.counter), &aad, &mut pt, tag.as_slice().into())
            .map_err(|_| ChatError::BadTag)?;
        slot.last = Some(sealed.counter);
        let text = String::from_utf8(pt).map_err(|_| ChatError::NotUtf8)?;
        if text.chars().count() > MAX_CHAT_CHARS {
            return Err(ChatError::Malformed);
        }
        Ok(text)
    }
}

/// Epoch a blob claims, for tests that check which key sealed it.
#[cfg(test)]
pub(crate) fn blob_epoch(blob: &[u8]) -> Option<u16> {
    postcard::from_bytes::<SealedChat>(blob).ok().map(|s| s.epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::MAX_CHAT_BLOB;

    use chacha20poly1305::aead::{Aead, Payload};

    const S: MemberId = 7;

    /// Seals arbitrary bytes the way a (hostile) sender could.
    fn seal_raw(k: &SenderKey, room: RoomId, sender: MemberId, counter: u32, pt: &[u8]) -> Vec<u8> {
        let aad = aad(room, sender, k.epoch, counter);
        let ct = cipher(k).encrypt(&nonce(counter), Payload { msg: pt, aad: &aad }).unwrap();
        postcard::to_allocvec(&SealedChat { epoch: k.epoch, counter, ct }).unwrap()
    }

    fn opener(keys: &[&SenderKey]) -> ChatOpener {
        let mut o = ChatOpener::new();
        for k in keys {
            o.add_key(k);
        }
        o
    }

    #[test]
    fn chat_from_other_room_does_not_open() {
        let k = SenderKey::generate(1);
        let blob = ChatSealer::new(&k).seal(2, S, "hej").unwrap();
        assert_eq!(opener(&[&k]).open(1, S, &blob), Err(ChatError::BadTag));
        assert_eq!(opener(&[&k]).open(2, S + 1, &blob), Err(ChatError::BadTag));
        assert_eq!(opener(&[&k]).open(2, S, &blob).as_deref(), Ok("hej"));
    }

    #[test]
    fn longest_chat_fits_blob_limit() {
        let k = SenderKey::generate(u16::MAX);
        let text = "😀".repeat(MAX_CHAT_CHARS);
        let mut sealer = ChatSealer::new(&k);
        sealer.counter = Some(u32::MAX);
        let blob = sealer.seal(u32::MAX, MemberId::MAX, &text).unwrap();
        assert!(blob.len() <= MAX_CHAT_BLOB, "{}", blob.len());
        assert_eq!(opener(&[&k]).open(u32::MAX, MemberId::MAX, &blob), Ok(text));
    }

    #[test]
    fn too_long_text_is_refused_both_ways() {
        let k = SenderKey::generate(1);
        let long = "a".repeat(MAX_CHAT_CHARS + 1);
        assert!(ChatSealer::new(&k).seal(1, S, &long).is_none());
        let blob = seal_raw(&k, 1, S, 0, long.as_bytes());
        assert_eq!(opener(&[&k]).open(1, S, &blob), Err(ChatError::Malformed));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let k = SenderKey::generate(1);
        let blob = seal_raw(&k, 1, S, 0, &[0xff, 0xfe]);
        assert_eq!(opener(&[&k]).open(1, S, &blob), Err(ChatError::NotUtf8));
    }

    #[test]
    fn counters_must_increase() {
        let k = SenderKey::generate(1);
        let mut s = ChatSealer::new(&k);
        let (m0, m1, m2) = (s.seal(1, S, "a").unwrap(), s.seal(1, S, "b").unwrap(), s.seal(1, S, "c").unwrap());
        let mut o = opener(&[&k]);
        assert_eq!(o.open(1, S, &m1).as_deref(), Ok("b"));
        assert_eq!(o.open(1, S, &m0), Err(ChatError::Replay));
        assert_eq!(o.open(1, S, &m1), Err(ChatError::Replay));
        let mut bad = m2.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(o.open(1, S, &bad), Err(ChatError::BadTag));
        assert_eq!(o.open(1, S, &m2).as_deref(), Ok("c"), "a failed open must not burn the counter");
    }

    #[test]
    fn sealer_stops_at_counter_limit() {
        let k = SenderKey::generate(1);
        let mut s = ChatSealer::new(&k);
        s.counter = Some(u32::MAX);
        assert!(s.seal(1, S, "last").is_some());
        assert!(s.seal(1, S, "over").is_none());
    }

    #[test]
    fn opener_keeps_three_newest_and_never_swaps_a_key() {
        let keys: Vec<_> = (1..=4).map(SenderKey::generate).collect();
        let blobs: Vec<_> = keys.iter().map(|k| ChatSealer::new(k).seal(1, S, "x").unwrap()).collect();
        let mut o = opener(&keys.iter().collect::<Vec<_>>());
        assert_eq!(o.open(1, S, &blobs[0]), Err(ChatError::UnknownEpoch));
        for b in &blobs[1..] {
            assert!(o.open(1, S, b).is_ok());
        }
        let evil = SenderKey::generate(4);
        o.add_key(&evil);
        let mut forger = ChatSealer::new(&evil);
        forger.counter = Some(5); // past the replay check, so only the key decides
        let forged = forger.seal(1, S, "y").unwrap();
        assert_eq!(o.open(1, S, &forged), Err(ChatError::BadTag));
    }

    #[test]
    fn junk_is_malformed() {
        let k = SenderKey::generate(1);
        let mut o = opener(&[&k]);
        for junk in [&[][..], &[1], &[1, 0, 3, 1, 2, 3], &[0xff; 40]] {
            assert_eq!(o.open(1, S, junk), Err(ChatError::Malformed), "{junk:?}");
        }
        let mut trailing = ChatSealer::new(&k).seal(1, S, "x").unwrap();
        trailing.push(0);
        assert_eq!(o.open(1, S, &trailing), Err(ChatError::Malformed));
        let short = postcard::to_allocvec(&SealedChat { epoch: k.epoch, counter: 0, ct: vec![0; TAG_LEN - 1] }).unwrap();
        assert_eq!(o.open(1, S, &short), Err(ChatError::Malformed), "ct shorter than the tag");
    }
}
