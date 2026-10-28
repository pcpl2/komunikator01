//! Pairwise Noise KK channels between two devices, relayed through the untrusted server.

use protocol::MAX_BLOB;
use serde::{Deserialize, Serialize};

use super::identity::{Identity, IdentityPublic};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairMsg {
    Hello1(Vec<u8>),
    Hello2(Vec<u8>),
    /// `nonce u64 LE ‖ ciphertext`.
    Data(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Inner {
    SenderKey { epoch: u16, key: [u8; 32] },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    Malformed,
    Handshake,
    NotReady,
    Unexpected,
    Decrypt,
    Replay,
    Exhausted,
}

const NOISE_PARAMS: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE_TAG: &[u8] = b"komunikator/pair/v1";
const NONCE_LEN: usize = 8;

/// Pairwise Noise KK channel. The device with the smaller `sign` key initiates.
///
/// After the handshake the session is converted to a stateless transport: every `Data` blob
/// carries an explicit `u64` nonce, and the receiver accepts only strictly increasing nonces,
/// so messages the server drops or reorders neither desynchronise nor replay.
pub struct PairChannel {
    initiator: bool,
    local_secret: [u8; 32],
    peer_dh: [u8; 32],
    prologue: Vec<u8>,
    /// Initiator only: handshake in flight after the latest `start()`.
    pending: Option<snow::HandshakeState>,
    /// Initiator only: the handshake of the `start()` before that. Over a slow relay the
    /// answer to an earlier `Hello1` can arrive after a re-send; it still completes.
    previous: Option<snow::HandshakeState>,
    session: Option<Session>,
}

struct Session {
    transport: snow::StatelessTransportState,
    send_nonce: u64,
    last_recv: Option<u64>,
}

impl PairChannel {
    pub fn new(me: &Identity, peer: &IdentityPublic) -> Self {
        let mine = me.public();
        debug_assert_ne!(mine.sign, peer.sign, "a device must not pair with itself");
        let initiator = mine.sign < peer.sign;
        let (first, second) = if initiator { (mine.sign, peer.sign) } else { (peer.sign, mine.sign) };
        let mut prologue = PROLOGUE_TAG.to_vec();
        prologue.extend_from_slice(&first);
        prologue.extend_from_slice(&second);
        Self {
            initiator,
            local_secret: *me.dh_secret(),
            peer_dh: peer.dh,
            prologue,
            pending: None,
            previous: None,
            session: None,
        }
    }

    fn fresh_handshake(&self) -> Result<snow::HandshakeState, PairError> {
        let b = snow::Builder::new(NOISE_PARAMS.parse().map_err(|_| PairError::Handshake)?)
            .prologue(&self.prologue)
            .map_err(|_| PairError::Handshake)?
            .local_private_key(&self.local_secret)
            .map_err(|_| PairError::Handshake)?
            .remote_public_key(&self.peer_dh)
            .map_err(|_| PairError::Handshake)?;
        if self.initiator { b.build_initiator() } else { b.build_responder() }
            .map_err(|_| PairError::Handshake)
    }

    /// Initiator: a fresh `Hello1` (callable again to restart). Responder: `None`.
    /// An already established session stays usable until the new handshake completes.
    pub fn start(&mut self) -> Option<Vec<u8>> {
        if !self.initiator {
            return None;
        }
        let mut hs = self.fresh_handshake().ok()?;
        let mut out = vec![0u8; MAX_BLOB];
        let n = hs.write_message(&[], &mut out).ok()?;
        out.truncate(n);
        self.previous = self.pending.replace(hs);
        encode(&PairMsg::Hello1(out))
    }

    pub fn is_ready(&self) -> bool {
        self.session.is_some()
    }

    /// Processes a blob from the peer: returns an optional reply and an optional decrypted message.
    pub fn receive(&mut self, blob: &[u8]) -> Result<(Option<Vec<u8>>, Option<Inner>), PairError> {
        if blob.len() > MAX_BLOB {
            return Err(PairError::Malformed);
        }
        let (msg, rest) = postcard::take_from_bytes::<PairMsg>(blob).map_err(|_| PairError::Malformed)?;
        if !rest.is_empty() {
            return Err(PairError::Malformed);
        }
        match msg {
            PairMsg::Hello1(h) if !self.initiator => self.on_hello1(&h).map(|r| (Some(r), None)),
            PairMsg::Hello2(h) if self.initiator && self.pending.is_some() => {
                self.on_hello2(&h).map(|()| (None, None))
            }
            PairMsg::Data(d) => self.on_data(&d).map(|i| (None, Some(i))),
            _ => Err(PairError::Unexpected),
        }
    }

    fn on_hello1(&mut self, h: &[u8]) -> Result<Vec<u8>, PairError> {
        // Any failure leaves an established session untouched.
        let mut hs = self.fresh_handshake()?;
        let mut scratch = vec![0u8; MAX_BLOB];
        hs.read_message(h, &mut scratch).map_err(|_| PairError::Handshake)?;
        let mut out = vec![0u8; MAX_BLOB];
        let n = hs.write_message(&[], &mut out).map_err(|_| PairError::Handshake)?;
        out.truncate(n);
        let reply = encode(&PairMsg::Hello2(out)).ok_or(PairError::Handshake)?;
        self.session = Some(Session::from_handshake(hs)?);
        Ok(reply)
    }

    /// Completes against the latest `Hello1`, else the one before it. A failed read leaves a
    /// handshake state as it was (snow restores it), so junk discards neither pending state.
    /// Answering the latest clears both; answering the previous keeps the latest, since the
    /// responder may still adopt it, and its answer then replaces the session.
    fn on_hello2(&mut self, h: &[u8]) -> Result<(), PairError> {
        let mut scratch = vec![0u8; MAX_BLOB];
        if let Some(hs) = self.pending.as_mut() {
            if hs.read_message(h, &mut scratch).is_ok() {
                let hs = self.pending.take().ok_or(PairError::Unexpected)?;
                self.previous = None;
                self.session = Some(Session::from_handshake(hs)?);
                return Ok(());
            }
        }
        if let Some(hs) = self.previous.as_mut() {
            if hs.read_message(h, &mut scratch).is_ok() {
                let hs = self.previous.take().ok_or(PairError::Unexpected)?;
                self.session = Some(Session::from_handshake(hs)?);
                return Ok(());
            }
        }
        Err(PairError::Handshake)
    }

    fn on_data(&mut self, d: &[u8]) -> Result<Inner, PairError> {
        let s = self.session.as_mut().ok_or(PairError::NotReady)?;
        if d.len() < NONCE_LEN {
            return Err(PairError::Malformed);
        }
        let (n, ct) = d.split_at(NONCE_LEN);
        let nonce = u64::from_le_bytes(n.try_into().map_err(|_| PairError::Malformed)?);
        if s.last_recv.is_some_and(|last| nonce <= last) {
            return Err(PairError::Replay);
        }
        let mut pt = vec![0u8; ct.len()];
        let len = s.transport.read_message(nonce, ct, &mut pt).map_err(|_| PairError::Decrypt)?;
        pt.truncate(len);
        let inner = postcard::from_bytes::<Inner>(&pt).map_err(|_| PairError::Malformed)?;
        s.last_recv = Some(nonce);
        Ok(inner)
    }

    pub fn send(&mut self, inner: &Inner) -> Result<Vec<u8>, PairError> {
        let s = self.session.as_mut().ok_or(PairError::NotReady)?;
        let pt = postcard::to_allocvec(inner).map_err(|_| PairError::Malformed)?;
        let nonce = s.send_nonce;
        if nonce == u64::MAX {
            return Err(PairError::Exhausted);
        }
        let mut data = vec![0u8; NONCE_LEN + pt.len() + 16];
        data[..NONCE_LEN].copy_from_slice(&nonce.to_le_bytes());
        let n = s
            .transport
            .write_message(nonce, &pt, &mut data[NONCE_LEN..])
            .map_err(|_| PairError::Exhausted)?;
        data.truncate(NONCE_LEN + n);
        s.send_nonce += 1;
        let blob = encode(&PairMsg::Data(data)).ok_or(PairError::Malformed)?;
        if blob.len() > MAX_BLOB {
            return Err(PairError::Malformed);
        }
        Ok(blob)
    }
}

impl Session {
    fn from_handshake(hs: snow::HandshakeState) -> Result<Self, PairError> {
        if !hs.is_handshake_finished() {
            return Err(PairError::Handshake);
        }
        let transport = hs.into_stateless_transport_mode().map_err(|_| PairError::Handshake)?;
        Ok(Self { transport, send_nonce: 0, last_recv: None })
    }
}

fn encode(m: &PairMsg) -> Option<Vec<u8>> {
    postcard::to_allocvec(m).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Identity, Identity) {
        (Identity::generate(), Identity::generate())
    }

    fn key_msg(n: u8) -> Inner {
        Inner::SenderKey { epoch: n as u16, key: [n; 32] }
    }

    fn enc(m: PairMsg) -> Vec<u8> {
        postcard::to_allocvec(&m).unwrap()
    }

    /// Fresh (initiator, responder) channels with their identities, in that role order.
    fn roles() -> ((Identity, PairChannel), (Identity, PairChannel)) {
        let (a, b) = pair();
        let a_inits = a.public().sign < b.public().sign;
        let (i, r) = if a_inits { (a, b) } else { (b, a) };
        let ci = PairChannel::new(&i, &r.public());
        let cr = PairChannel::new(&r, &i.public());
        ((i, ci), (r, cr))
    }

    fn ready_pair() -> (PairChannel, PairChannel) {
        let ((_, mut ci), (_, mut cr)) = roles();
        let h1 = ci.start().unwrap();
        assert!(cr.start().is_none());
        let (h2, inner) = cr.receive(&h1).unwrap();
        assert!(inner.is_none());
        ci.receive(&h2.unwrap()).unwrap();
        (ci, cr)
    }

    #[test]
    fn exactly_one_side_initiates() {
        for _ in 0..8 {
            let (a, b) = pair();
            let mut ca = PairChannel::new(&a, &b.public());
            let mut cb = PairChannel::new(&b, &a.public());
            assert!(ca.start().is_some() ^ cb.start().is_some());
        }
    }

    #[test]
    fn handshake_then_sender_key_both_ways() {
        let (mut ca, mut cb) = ready_pair();
        assert!(ca.is_ready() && cb.is_ready());
        let blob = ca.send(&key_msg(1)).unwrap();
        assert!(blob.len() <= MAX_BLOB);
        assert_eq!(cb.receive(&blob).unwrap(), (None, Some(key_msg(1))));
        let blob = cb.send(&key_msg(2)).unwrap();
        assert_eq!(ca.receive(&blob).unwrap(), (None, Some(key_msg(2))));
    }

    /// An identity whose `sign` sorts below `r`'s, so a responder `r` still treats it as initiator.
    fn identity_below(r: &Identity) -> Identity {
        loop {
            let o = Identity::generate();
            if o.public().sign < r.public().sign {
                return o;
            }
        }
    }

    #[test]
    fn wrong_peer_key_fails() {
        let ((i, mut ci), (r, _)) = roles();
        let _ = i;
        // The responder expects a different initiator (still sorting as initiator).
        let other = identity_below(&r);
        let mut cr = PairChannel::new(&r, &other.public());
        let h1 = ci.start().unwrap();
        assert_eq!(cr.receive(&h1).unwrap_err(), PairError::Handshake);
        assert!(!cr.is_ready());
    }

    #[test]
    fn prologue_binds_sign_keys() {
        let ((i, mut ci), (r, _)) = roles();
        // Correct dh key, but a different sign key for the expected initiator.
        let other = identity_below(&r);
        let mut wrong = i.public();
        wrong.sign = other.public().sign;
        let mut cr = PairChannel::new(&r, &wrong);
        let h1 = ci.start().unwrap();
        assert_eq!(cr.receive(&h1).unwrap_err(), PairError::Handshake);
        assert!(!cr.is_ready());
    }

    #[test]
    fn tampered_data_fails() {
        let (mut ca, mut cb) = ready_pair();
        let mut blob = ca.send(&key_msg(1)).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(cb.receive(&blob).is_err());
        // A failed decryption does not burn the nonce: the genuine message still arrives.
        blob[last] ^= 1;
        assert!(cb.receive(&blob).is_ok());
    }

    #[test]
    fn data_before_ready_rejected() {
        let (mut ready_a, _ready_b) = ready_pair();
        let data = ready_a.send(&key_msg(1)).unwrap();
        let (a, b) = pair();
        let mut ca = PairChannel::new(&a, &b.public());
        let mut cb = PairChannel::new(&b, &a.public());
        assert!(ca.receive(&data).is_err());
        assert!(cb.receive(&data).is_err());
        assert!(ca.send(&key_msg(1)).is_err());
        assert!(!ca.is_ready());
    }

    #[test]
    fn replay_and_reorder_rejected_but_gaps_allowed() {
        let (mut ca, mut cb) = ready_pair();
        let m1 = ca.send(&key_msg(1)).unwrap();
        let _m2 = ca.send(&key_msg(2)).unwrap(); // dropped by the server
        let m3 = ca.send(&key_msg(3)).unwrap();
        assert!(cb.receive(&m3).is_ok());
        assert!(cb.receive(&m3).is_err(), "replay");
        assert!(cb.receive(&m1).is_err(), "reorder");
    }

    #[test]
    fn malformed_input_never_panics() {
        let (mut ca, mut cb) = ready_pair();
        let cases: [&[u8]; 6] = [&[], &[0xff], &[2], &[2, 3, 1, 2, 3], &[0, 200], &[2, 7, 0, 0, 0, 0, 0, 0, 0]];
        for blob in cases {
            assert!(ca.receive(blob).is_err());
            assert!(cb.receive(blob).is_err());
        }
        assert!(ca.receive(&vec![0u8; MAX_BLOB + 1]).is_err());
        let blob = ca.send(&key_msg(9)).unwrap();
        assert!(cb.receive(&blob).is_ok());
    }

    #[test]
    fn failed_handshake_can_restart() {
        let ((_, mut init), (_, mut resp)) = roles();
        init.start().unwrap();
        assert!(init.receive(&enc(PairMsg::Hello2(vec![7; 48]))).is_err());
        assert!(!init.is_ready());
        let h1 = init.start().expect("initiator restarts");
        let (h2, _) = resp.receive(&h1).unwrap();
        init.receive(&h2.unwrap()).unwrap();
        assert!(init.is_ready() && resp.is_ready());
        let blob = init.send(&key_msg(3)).unwrap();
        assert_eq!(resp.receive(&blob).unwrap().1, Some(key_msg(3)));
    }

    #[test]
    fn hello2_for_previous_hello1_completes_after_restart() {
        let ((_, mut init), (_, mut resp)) = roles();
        let h1a = init.start().unwrap();
        let h1b = init.start().unwrap(); // restarted before the answer came back
        let (h2a, _) = resp.receive(&h1a).unwrap();
        init.receive(&h2a.unwrap()).expect("answer to the previous Hello1 completes");
        assert!(init.is_ready());
        // The responder then adopts the newer Hello1; its answer still completes too.
        let (h2b, _) = resp.receive(&h1b).unwrap();
        init.receive(&h2b.unwrap()).expect("answer to the newer Hello1 completes");
        let blob = init.send(&key_msg(4)).unwrap();
        assert_eq!(resp.receive(&blob).unwrap().1, Some(key_msg(4)));
        let blob = resp.send(&key_msg(5)).unwrap();
        assert_eq!(init.receive(&blob).unwrap().1, Some(key_msg(5)));
    }

    #[test]
    fn failed_hello2_keeps_both_pending_handshakes() {
        let ((_, mut init), (_, mut resp)) = roles();
        let h1a = init.start().unwrap();
        init.start().unwrap();
        assert!(init.receive(&enc(PairMsg::Hello2(vec![7; 48]))).is_err());
        let (h2a, _) = resp.receive(&h1a).unwrap();
        init.receive(&h2a.unwrap()).expect("junk Hello2 did not discard the previous state");
        assert!(init.is_ready());
    }

    #[test]
    fn responder_accepts_new_hello1_after_ready() {
        let ((i, mut init), (r, mut resp)) = roles();
        let h1 = init.start().unwrap();
        let (h2, _) = resp.receive(&h1).unwrap();
        init.receive(&h2.unwrap()).unwrap();
        assert!(resp.is_ready());
        // The initiator restarts with a brand-new channel.
        let mut init2 = PairChannel::new(&i, &r.public());
        let h1 = init2.start().unwrap();
        let (h2, _) = resp.receive(&h1).unwrap();
        init2.receive(&h2.unwrap()).unwrap();
        let blob = init2.send(&key_msg(5)).unwrap();
        assert_eq!(resp.receive(&blob).unwrap().1, Some(key_msg(5)));
        let blob = resp.send(&key_msg(6)).unwrap();
        assert_eq!(init2.receive(&blob).unwrap().1, Some(key_msg(6)));
    }

    #[test]
    fn stray_hellos_leave_session_untouched() {
        let (mut init, mut resp) = ready_pair();
        // Hello1 at an initiator, Hello2 at a responder or a ready initiator: errors only.
        assert!(init.receive(&enc(PairMsg::Hello1(vec![1; 48]))).is_err());
        assert!(init.receive(&enc(PairMsg::Hello2(vec![1; 48]))).is_err());
        assert!(resp.receive(&enc(PairMsg::Hello2(vec![1; 48]))).is_err());
        assert!(init.is_ready() && resp.is_ready());
        let blob = init.send(&key_msg(1)).unwrap();
        assert!(resp.receive(&blob).is_ok());
        let blob = resp.send(&key_msg(2)).unwrap();
        assert!(init.receive(&blob).is_ok());
    }
}
