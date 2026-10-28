//! Room key agreement: a sans-IO state machine tying signed announcements, TOFU trust,
//! pairwise Noise channels and sender keys together (spec section 6.3).
//!
//! The server is treated as hostile. Invariants:
//! - my sender key leaves only over a ready pairwise channel, to a current member whose latest
//!   announce verified and whose trust is not `KeyChanged`;
//! - an identity never receives a key of mine that sealed audio before its link opened
//!   (`Peer::min_epoch`, enforced in `open_link`), so it cannot open earlier frames;
//! - a member's sender keys are accepted only from that member's own channel;
//! - every join/leave rotates my key; the sealer switches 500 ms later so peers have the new
//!   key before frames use it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use protocol::{ClientMsg, MemberId, MemberInfo};

use super::announce::{sign_announce, verify_announce, AnnounceError};
use super::audio::{AudioOpener, AudioSealer, SenderKey};
use super::identity::{Identity, IdentityPublic};
use super::pair::{Inner, PairChannel, PairError};
use super::trust::{safety_code, KnownDevices, Trust};

/// Delay between generating a new sender key and sealing with it.
const SWITCH_DELAY: Duration = Duration::from_millis(500);
/// Minimum spacing of `Hello1`s to one member (restarts and retries), so a hostile server
/// cannot make us loop at full speed.
const HELLO_INTERVAL: Duration = Duration::from_secs(1);
/// Cap of the exponential backoff (1, 2, 4, 8 s) between `Hello1` re-sends while a link is
/// not ready, so a relay round trip above `HELLO_INTERVAL` still lets an answer land.
const MAX_HELLO_INTERVAL: Duration = Duration::from_secs(8);
/// A coalesced rotation may postpone the switch at most this long past its first deadline, so
/// continuous churn cannot keep the sealer on a key that departed members hold.
const MAX_DEFER: Duration = Duration::from_secs(1);
/// Re-announce period: keeps the server's cached announce (replayed to newcomers) far from
/// `ANNOUNCE_MAX_SKEW_MS`.
const REANNOUNCE: Duration = Duration::from_secs(30 * 60);
/// Rooms are not implemented yet; every announce is for room 0.
const ROOM: u32 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum E2eEvent {
    Member { id: MemberId, name: String, identity: IdentityPublic, trust: Trust, code: String },
    Rejected { id: MemberId, reason: AnnounceError },
}

#[derive(Debug, Default)]
pub struct Output {
    pub send: Vec<ClientMsg>,
    pub events: Vec<E2eEvent>,
    /// Test log of sender keys sent: (recipient, epoch).
    #[cfg(test)]
    pub keys_sent: Vec<(MemberId, u16)>,
}

struct Pending {
    key: SenderKey,
    /// Deadline set by the first change of this rotation; coalescing never defers past
    /// `first_due + MAX_DEFER`.
    first_due: Instant,
    switch_at: Instant,
}

struct Peer {
    name: String,
    /// Oldest epoch of my sender keys this member may receive.
    min_epoch: u16,
    /// Identity from the member's latest announce, if it verified.
    identity: Option<IdentityPublic>,
    /// Present only while the member is eligible (verified, not `KeyChanged`).
    link: Option<Link>,
}

struct Link {
    channel: PairChannel,
    /// Holds only keys received over `channel`.
    opener: AudioOpener,
    initiator: bool,
    last_hello: Option<Instant>,
    /// Minimum spacing before the next `Hello1`: `HELLO_INTERVAL`, doubled on each re-send
    /// while not ready (up to `MAX_HELLO_INTERVAL`), reset when the handshake completes.
    hello_interval: Duration,
    /// Initiator only: a `Hello1` is owed but was rate-limited.
    restart_due: bool,
}

pub struct E2e {
    identity: Arc<Identity>,
    my_public: IdentityPublic,
    trust: Arc<Mutex<KnownDevices>>,
    me: MemberId,
    my_name: String,
    peers: HashMap<MemberId, Peer>,
    current: SenderKey,
    sealer: AudioSealer,
    pending: Option<Pending>,
    /// Latest time seen from the caller, for `trust_changed` (which takes none).
    clock: Instant,
    /// `(instant, unix ms)` at construction, to derive wall-clock time for re-announces.
    base: (Instant, u64),
    last_announce: Instant,
}

/// `e` is `min` or newer in wrapping u16 epoch order.
fn at_least(e: u16, min: u16) -> bool {
    (e.wrapping_sub(min) as i16) >= 0
}

fn relay(out: &mut Output, to: MemberId, blob: Vec<u8>) {
    out.send.push(ClientMsg::Relay { to, blob });
}

fn send_key(out: &mut Output, to: MemberId, link: &mut Link, min_epoch: u16, key: &SenderKey) {
    if !link.channel.is_ready() || !at_least(key.epoch, min_epoch) {
        return;
    }
    if let Ok(blob) = link.channel.send(&Inner::SenderKey { epoch: key.epoch, key: *key.secret() }) {
        relay(out, to, blob);
        #[cfg(test)]
        out.keys_sent.push((to, key.epoch));
    }
}

/// Initiator: sends a fresh `Hello1` unless one went out less than `hello_interval` ago, in
/// which case it is remembered and sent from `tick`. A re-send on a link that is still not
/// ready doubles the interval (backoff), so slow relays are not flooded with restarts.
fn hello(out: &mut Output, to: MemberId, link: &mut Link, now: Instant) {
    if !link.initiator {
        return;
    }
    if link.last_hello.is_some_and(|t| now.saturating_duration_since(t) < link.hello_interval) {
        link.restart_due = true;
        return;
    }
    if link.last_hello.is_some() && !link.channel.is_ready() {
        link.hello_interval = (link.hello_interval * 2).min(MAX_HELLO_INTERVAL);
    }
    link.last_hello = Some(now);
    link.restart_due = false;
    if let Some(h1) = link.channel.start() {
        relay(out, to, h1);
    }
}

impl Peer {
    fn new(name: String, min_epoch: u16) -> Self {
        Self { name, min_epoch, identity: None, link: None }
    }
}

impl E2e {
    /// Joins the room: `members` as listed by the server's `Welcome` (I may be included). The
    /// returned `Output` carries my signed announce; my first sender key has epoch `next_epoch`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: Arc<Identity>,
        trust: Arc<Mutex<KnownDevices>>,
        me: MemberId,
        my_name: String,
        members: &[MemberInfo],
        next_epoch: u16,
        now: Instant,
        now_ms: u64,
    ) -> (Self, Output) {
        let current = SenderKey::generate(next_epoch);
        let peers = members
            .iter()
            .filter(|m| m.id != me)
            .map(|m| {
                let peer = Peer::new(m.name.clone(), next_epoch);
                (m.id, peer)
            })
            .collect();
        let out = Output {
            send: vec![ClientMsg::Announce { blob: sign_announce(&identity, &my_name, ROOM, now_ms) }],
            ..Output::default()
        };
        let e2e = Self {
            my_public: identity.public(),
            identity,
            trust,
            me,
            my_name,
            peers,
            sealer: AudioSealer::new(me, &current),
            current,
            pending: None,
            clock: now,
            base: (now, now_ms),
            last_announce: now,
        };
        (e2e, out)
    }

    fn observe(&mut self, now: Instant) {
        self.clock = self.clock.max(now);
    }

    /// Broadcasts a freshly signed announce (wall clock derived from the construction base).
    fn announce_self(&mut self, now: Instant, out: &mut Output) {
        let (base, base_ms) = self.base;
        let now_ms = base_ms.saturating_add(now.saturating_duration_since(base).as_millis() as u64);
        let blob = sign_announce(&self.identity, &self.my_name, ROOM, now_ms);
        out.send.push(ClientMsg::Announce { blob });
        self.last_announce = now;
    }

    fn trust_store(&self) -> MutexGuard<'_, KnownDevices> {
        self.trust.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Epoch of my newest sender key (pending or current).
    fn newest_epoch(&self) -> u16 {
        self.pending.as_ref().map_or(self.current.epoch, |p| p.key.epoch)
    }

    /// Generates a fresh pending key one epoch past the newest, (re)starts the switch timer and
    /// sends the key to every ready, eligible channel. A previous pending key is discarded unused.
    fn rotate(&mut self, now: Instant, out: &mut Output) {
        let key = SenderKey::generate(self.newest_epoch().wrapping_add(1));
        for (&id, peer) in &mut self.peers {
            if let Some(link) = peer.link.as_mut() {
                send_key(out, id, link, peer.min_epoch, &key);
            }
        }
        let fresh = now + SWITCH_DELAY;
        let (first_due, switch_at) = match &self.pending {
            Some(p) => (p.first_due, fresh.min(p.first_due + MAX_DEFER)),
            None => (fresh, fresh),
        };
        self.pending = Some(Pending { key, first_due, switch_at });
    }

    /// Rotates and raises `id`'s floor to the new key: called when an identity that may hold my
    /// keys stops being the one I talk to, so whoever comes next gets only keys made after.
    fn reset_floor(&mut self, id: MemberId, now: Instant, out: &mut Output) {
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.link = None;
        }
        self.rotate(now, out);
        let newest = self.newest_epoch();
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.min_epoch = newest;
        }
    }

    fn member_event(&self, id: MemberId, trust: Trust) -> Option<E2eEvent> {
        let peer = self.peers.get(&id)?;
        let identity = peer.identity.as_ref()?;
        Some(E2eEvent::Member {
            id,
            name: peer.name.clone(),
            identity: *identity,
            trust,
            code: safety_code(&self.my_public, identity),
        })
    }

    /// Creates the pairwise channel to an eligible member; the initiator sends `Hello1`.
    ///
    /// Invariant: an identity never receives a key that sealed audio before its link opened.
    /// If the member's floor is a key I already seal with (or an older one), rotate first and
    /// raise the floor to the fresh, still unused key. A floor on the unused pending key (the
    /// usual join, then announce within 500 ms) needs nothing.
    fn open_link(&mut self, id: MemberId, now: Instant, out: &mut Output) {
        let Some(peer) = self.peers.get(&id) else { return };
        if peer.identity.is_none() {
            return;
        }
        if at_least(self.current.epoch, peer.min_epoch) {
            self.reset_floor(id, now, out);
        }
        let Some(peer) = self.peers.get_mut(&id) else { return };
        let Some(their) = peer.identity else { return };
        let mut link = Link {
            channel: PairChannel::new(&self.identity, &their),
            opener: AudioOpener::new(id),
            initiator: self.my_public.sign < their.sign,
            last_hello: None,
            hello_interval: HELLO_INTERVAL,
            restart_due: false,
        };
        hello(out, id, &mut link, now);
        peer.link = Some(link);
    }

    pub fn member_joined(&mut self, m: &MemberInfo, now: Instant) -> Output {
        self.observe(now);
        let mut out = Output::default();
        if m.id == self.me {
            return out;
        }
        // Rotate first: the newcomer's floor is the fresh key, never one it could have recorded.
        self.rotate(now, &mut out);
        self.peers.insert(m.id, Peer::new(m.name.clone(), self.newest_epoch()));
        // The server replays cached announces to the newcomer; make sure mine is fresh.
        self.announce_self(now, &mut out);
        out
    }

    pub fn member_left(&mut self, id: MemberId, now: Instant) -> Output {
        self.observe(now);
        let mut out = Output::default();
        if self.peers.remove(&id).is_some() {
            self.rotate(now, &mut out);
        }
        out
    }

    pub fn announce(&mut self, from: MemberId, blob: &[u8], now: Instant, now_ms: u64) -> Output {
        self.observe(now);
        let mut out = Output::default();
        let Some(peer) = self.peers.get(&from) else { return out };
        let verified = verify_announce(blob, &peer.name, ROOM, now_ms).and_then(|a| {
            if a.identity.sign == self.my_public.sign {
                Err(AnnounceError::OwnIdentity)
            } else {
                Ok(a.identity)
            }
        });
        let identity = match verified {
            Ok(identity) => identity,
            Err(reason) => {
                let Some(peer) = self.peers.get_mut(&from) else { return out };
                peer.link = None;
                let had_identity = peer.identity.take().is_some();
                out.events.push(E2eEvent::Rejected { id: from, reason });
                if had_identity {
                    // It may hold my current key: keep future audio from it, and from whatever
                    // identity this member presents next.
                    self.reset_floor(from, now, &mut out);
                }
                return out;
            }
        };
        if peer.identity == Some(identity) {
            return out;
        }
        let changed = peer.identity.is_some();
        let name = peer.name.clone();
        let trust = self.trust_store().check(&name, &identity);
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.identity = Some(identity);
            peer.link = None;
        }
        if changed {
            // A different device now speaks for this member: it gets only keys from now on.
            self.reset_floor(from, now, &mut out);
        }
        out.events.extend(self.member_event(from, trust));
        if trust != Trust::KeyChanged {
            self.open_link(from, now, &mut out);
        }
        out
    }

    pub fn relay(&mut self, from: MemberId, blob: &[u8], now: Instant) -> Output {
        self.observe(now);
        let mut out = Output::default();
        let Some(peer) = self.peers.get_mut(&from) else { return out };
        let min_epoch = peer.min_epoch;
        let Some(link) = peer.link.as_mut() else { return out };
        match link.channel.receive(blob) {
            Ok((reply, Some(Inner::SenderKey { epoch, key }))) => {
                if let Some(r) = reply {
                    relay(&mut out, from, r);
                }
                link.opener.add_key(SenderKey::from_parts(epoch, key));
            }
            Ok((reply, None)) => {
                // A handshake completed (a new session): (re)send my keys over it.
                if let Some(r) = reply {
                    relay(&mut out, from, r);
                }
                link.restart_due = false;
                link.hello_interval = HELLO_INTERVAL;
                send_key(&mut out, from, link, min_epoch, &self.current);
                if let Some(p) = &self.pending {
                    send_key(&mut out, from, link, min_epoch, &p.key);
                }
            }
            // Junk or a replay says nothing about our session.
            Err(PairError::Malformed | PairError::Replay | PairError::Exhausted) => {}
            // The peer holds a session we cannot use (e.g. after a stale or replayed Hello1):
            // the initiator starts over (rate-limited). A responder waits for the next Hello1.
            Err(_) => hello(&mut out, from, link, now),
        }
        out
    }

    /// Re-reads trust for `id` after `KnownDevices::accept`/`verify`.
    pub fn trust_changed(&mut self, id: MemberId) -> Output {
        let mut out = Output::default();
        let Some(peer) = self.peers.get(&id) else { return out };
        let Some(identity) = peer.identity else { return out };
        let name = peer.name.clone();
        let trust = self.trust_store().check(&name, &identity);
        out.events.extend(self.member_event(id, trust));
        let has_link = self.peers.get(&id).is_some_and(|p| p.link.is_some());
        if trust == Trust::KeyChanged {
            if has_link {
                self.reset_floor(id, self.clock, &mut out);
            }
        } else if !has_link {
            self.open_link(id, self.clock, &mut out);
        }
        out
    }

    pub fn tick(&mut self, now: Instant) -> Output {
        self.observe(now);
        let mut out = Output::default();
        if self.pending.as_ref().is_some_and(|p| now >= p.switch_at) {
            if let Some(p) = self.pending.take() {
                // Exactly one sealer per key: this key never had one.
                self.sealer = AudioSealer::new(self.me, &p.key);
                self.current = p.key;
            }
        }
        if now.saturating_duration_since(self.last_announce) >= REANNOUNCE {
            self.announce_self(now, &mut out);
        }
        for (&id, peer) in &mut self.peers {
            if let Some(link) = peer.link.as_mut() {
                if link.initiator && (link.restart_due || !link.channel.is_ready()) {
                    hello(&mut out, id, link, now);
                }
            }
        }
        out
    }

    /// Seals with the current (switched) key; returns its epoch.
    pub fn seal(&mut self, seq: u32, opus: &[u8], out: &mut Vec<u8>) -> Option<u16> {
        self.sealer.seal(seq, opus, out)
    }

    pub fn open(
        &mut self,
        sender: MemberId,
        epoch: u16,
        seq: u32,
        ct: &[u8],
        now: Instant,
        out: &mut Vec<u8>,
    ) -> bool {
        match self.peers.get_mut(&sender).and_then(|p| p.link.as_mut()) {
            Some(link) => link.opener.open(epoch, seq, ct, now, out),
            None => false,
        }
    }

    #[cfg(test)]
    fn current_key(&self) -> &SenderKey {
        &self.current
    }

    /// First epoch for the next connection, so keys never repeat an epoch across reconnects.
    pub fn next_epoch(&self) -> u16 {
        self.newest_epoch().wrapping_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::time::Duration;

    use protocol::ServerMsg;

    const T0_MS: u64 = 1_800_000_000_000;

    struct Client {
        e2e: E2e,
        identity: Arc<Identity>,
        trust: Arc<Mutex<KnownDevices>>,
        _dir: Arc<tempfile::TempDir>,
        name: String,
        inbox: VecDeque<ServerMsg>,
        events: Vec<E2eEvent>,
        seq: u32,
    }

    struct Frame {
        epoch: u16,
        seq: u32,
        ct: Vec<u8>,
    }

    /// In-memory server: ordered per-recipient queues, stamps `from`, caches and broadcasts
    /// announces, delivers relays only to `to`.
    struct Net {
        now: Instant,
        now_ms: u64,
        next_id: MemberId,
        clients: BTreeMap<MemberId, Client>,
        gone: BTreeMap<MemberId, Client>,
        announces: BTreeMap<MemberId, Vec<u8>>,
        relays: Vec<(MemberId, MemberId, Vec<u8>)>,
        /// (from, to, epoch) of every sender key sent.
        keys: Vec<(MemberId, MemberId, u16)>,
        /// One-way delay of relayed blobs (zero: delivered at once).
        relay_delay: Duration,
        /// Relays held back by `relay_delay`: (due, to, message), in send order.
        in_flight: Vec<(Instant, MemberId, ServerMsg)>,
    }

    fn store() -> (Arc<tempfile::TempDir>, Arc<Mutex<KnownDevices>>) {
        let dir = tempfile::tempdir().unwrap();
        let k = KnownDevices::load(&dir.path().join("known.toml")).unwrap();
        (Arc::new(dir), Arc::new(Mutex::new(k)))
    }

    impl Net {
        fn new() -> Self {
            Net {
                now: Instant::now(),
                now_ms: T0_MS,
                next_id: 1,
                clients: BTreeMap::new(),
                gone: BTreeMap::new(),
                announces: BTreeMap::new(),
                relays: Vec::new(),
                keys: Vec::new(),
                relay_delay: Duration::ZERO,
                in_flight: Vec::new(),
            }
        }

        fn join(&mut self, name: &str) -> MemberId {
            let (dir, trust) = store();
            self.join_with(name, Arc::new(Identity::generate()), trust, dir, 0)
        }

        /// The departed client `old` comes back (same identity and trust store, new id).
        fn rejoin(&mut self, old: MemberId) -> MemberId {
            let c = self.gone.remove(&old).unwrap();
            let epoch = c.e2e.next_epoch();
            self.join_with(&c.name, c.identity, c.trust, c._dir, epoch)
        }

        fn join_with(
            &mut self,
            name: &str,
            identity: Arc<Identity>,
            trust: Arc<Mutex<KnownDevices>>,
            dir: Arc<tempfile::TempDir>,
            next_epoch: u16,
        ) -> MemberId {
            let id = self.next_id;
            self.next_id += 1;
            let info = MemberInfo { id, name: name.into() };
            let mut members = Vec::new();
            for (&cid, c) in self.clients.iter_mut() {
                c.inbox.push_back(ServerMsg::MemberJoined(info.clone()));
                members.push(MemberInfo { id: cid, name: c.name.clone() });
            }
            let (e2e, out) = E2e::new(
                identity.clone(),
                trust.clone(),
                id,
                name.into(),
                &members,
                next_epoch,
                self.now,
                self.now_ms,
            );
            let inbox = self
                .announces
                .iter()
                .map(|(&from, blob)| ServerMsg::Announce { from, blob: blob.clone() })
                .collect();
            let c = Client {
                e2e,
                identity,
                trust,
                _dir: dir,
                name: name.into(),
                inbox,
                events: Vec::new(),
                seq: 0,
            };
            self.clients.insert(id, c);
            self.route(id, out);
            id
        }

        /// The server hands member `id` (same id and name) to a brand-new device.
        fn hijack(&mut self, id: MemberId) {
            let old = self.clients.remove(&id).unwrap();
            self.announces.remove(&id);
            let members: Vec<_> =
                self.clients.iter().map(|(&cid, c)| MemberInfo { id: cid, name: c.name.clone() }).collect();
            let (dir, trust) = store();
            let identity = Arc::new(Identity::generate());
            let (e2e, out) =
                E2e::new(identity.clone(), trust.clone(), id, old.name.clone(), &members, 0, self.now, self.now_ms);
            let inbox = self
                .announces
                .iter()
                .map(|(&from, blob)| ServerMsg::Announce { from, blob: blob.clone() })
                .collect();
            let name = old.name.clone();
            self.gone.insert(id, old);
            let c = Client { e2e, identity, trust, _dir: dir, name, inbox, events: Vec::new(), seq: 0 };
            self.clients.insert(id, c);
            self.route(id, out);
        }

        fn leave(&mut self, id: MemberId) {
            let c = self.clients.remove(&id).unwrap();
            self.gone.insert(id, c);
            self.announces.remove(&id);
            for c in self.clients.values_mut() {
                c.inbox.push_back(ServerMsg::MemberLeft { id });
            }
        }

        fn route(&mut self, from: MemberId, out: Output) {
            self.keys.extend(out.keys_sent.iter().map(|&(to, e)| (from, to, e)));
            if let Some(c) = self.clients.get_mut(&from) {
                c.events.extend(out.events);
            }
            for msg in out.send {
                match msg {
                    ClientMsg::Announce { blob } => {
                        self.announces.insert(from, blob.clone());
                        for (_, c) in self.clients.iter_mut().filter(|(&id, _)| id != from) {
                            c.inbox.push_back(ServerMsg::Announce { from, blob: blob.clone() });
                        }
                    }
                    ClientMsg::Relay { to, blob } => {
                        self.relays.push((from, to, blob.clone()));
                        if !self.relay_delay.is_zero() {
                            self.in_flight.push((self.now + self.relay_delay, to, ServerMsg::Relay { from, blob }));
                        } else if let Some(c) = self.clients.get_mut(&to).filter(|_| to != from) {
                            c.inbox.push_back(ServerMsg::Relay { from, blob });
                        }
                    }
                    ClientMsg::Hello { .. } => {}
                }
            }
        }

        /// Takes every queued announce from `from` out of `to`'s inbox (the server holds them back).
        fn withhold(&mut self, to: MemberId, from: MemberId) -> Vec<ServerMsg> {
            let inbox = &mut self.clients.get_mut(&to).unwrap().inbox;
            let (held, kept): (Vec<_>, Vec<_>) = inbox
                .drain(..)
                .partition(|m| matches!(m, ServerMsg::Announce { from: f, .. } if *f == from));
            inbox.extend(kept);
            held
        }

        fn inject(&mut self, to: MemberId, msg: ServerMsg) {
            self.clients.get_mut(&to).unwrap().inbox.push_back(msg);
        }

        fn deliver(&mut self) {
            for _ in 0..100_000 {
                let Some((id, msg)) =
                    self.clients.iter_mut().find_map(|(&id, c)| c.inbox.pop_front().map(|m| (id, m)))
                else {
                    return;
                };
                let (now, now_ms) = (self.now, self.now_ms);
                let e = &mut self.clients.get_mut(&id).unwrap().e2e;
                let out = match msg {
                    ServerMsg::MemberJoined(m) => e.member_joined(&m, now),
                    ServerMsg::MemberLeft { id } => e.member_left(id, now),
                    ServerMsg::Announce { from, blob } => e.announce(from, &blob, now, now_ms),
                    ServerMsg::Relay { from, blob } => e.relay(from, &blob, now),
                    _ => Output::default(),
                };
                self.route(id, out);
            }
            panic!("messages did not settle");
        }

        /// Advances time by `ms`, ticks every client and delivers everything.
        fn tick(&mut self, ms: u64) {
            self.now += Duration::from_millis(ms);
            self.now_ms += ms;
            let now = self.now;
            let (due, later): (Vec<_>, Vec<_>) =
                std::mem::take(&mut self.in_flight).into_iter().partition(|(t, _, _)| *t <= now);
            self.in_flight = later;
            for (_, to, msg) in due {
                if let Some(c) = self.clients.get_mut(&to) {
                    c.inbox.push_back(msg);
                }
            }
            self.deliver();
            let ids: Vec<_> = self.clients.keys().copied().collect();
            for id in ids {
                let out = self.clients.get_mut(&id).unwrap().e2e.tick(self.now);
                self.route(id, out);
            }
            self.deliver();
        }

        fn call(&mut self, id: MemberId, f: impl FnOnce(&mut E2e, Instant, u64) -> Output) {
            let out = f(&mut self.clients.get_mut(&id).unwrap().e2e, self.now, self.now_ms);
            self.route(id, out);
        }

        fn client(&mut self, id: MemberId) -> &mut Client {
            self.clients.get_mut(&id).or_else(|| self.gone.get_mut(&id)).unwrap()
        }

        fn frame(&mut self, from: MemberId) -> Frame {
            let c = self.client(from);
            c.seq += 1;
            let mut ct = Vec::new();
            let epoch = c.e2e.seal(c.seq, b"opus", &mut ct).unwrap();
            Frame { epoch, seq: c.seq, ct }
        }

        fn opens(&mut self, who: MemberId, from: MemberId, f: &Frame) -> bool {
            let now = self.now;
            let mut pt = Vec::new();
            self.client(who).e2e.open(from, f.epoch, f.seq, &f.ct, now, &mut pt) && pt == b"opus"
        }

        /// Seals one frame from `from` and tries to open it at every other present client.
        fn speak(&mut self, from: MemberId) -> Vec<(MemberId, bool)> {
            let f = self.frame(from);
            let ids: Vec<_> = self.clients.keys().copied().filter(|&id| id != from).collect();
            ids.into_iter().map(|id| (id, self.opens(id, from, &f))).collect()
        }

        fn all_hear(&mut self) -> bool {
            let ids: Vec<_> = self.clients.keys().copied().collect();
            ids.into_iter().all(|id| self.speak(id).iter().all(|&(_, h)| h))
        }
    }

    fn room(names: &[&str]) -> (Net, Vec<MemberId>) {
        let mut net = Net::new();
        let ids = names
            .iter()
            .map(|n| {
                let id = net.join(n);
                net.deliver();
                id
            })
            .collect();
        net.tick(600);
        (net, ids)
    }

    #[test]
    fn slow_relay_still_pairs_without_hello_storm() {
        let mut net = Net::new();
        net.relay_delay = Duration::from_millis(1500); // round trip 3 s, above HELLO_INTERVAL
        let a = net.join("a");
        net.deliver();
        let b = net.join("b");
        net.deliver();
        for _ in 0..300 {
            net.tick(100);
        }
        assert!(net.all_hear(), "a pair with a 1.5 s relay delay becomes ready");
        let settled = net.relays.len();
        for _ in 0..300 {
            net.tick(100);
        }
        assert_eq!(net.relays.len(), settled, "no Hello1 retries once ready");
        let _ = (a, b);
    }

    #[test]
    fn three_members_hear_each_other() {
        let (mut net, ids) = room(&["a", "b", "c"]);
        assert!(net.all_hear());
        for &id in &ids {
            let members: Vec<_> = net
                .client(id)
                .events
                .iter()
                .filter_map(|e| match e {
                    E2eEvent::Member { id, trust: Trust::NewUser, .. } => Some(*id),
                    _ => None,
                })
                .collect();
            assert_eq!(members.len(), 2, "{id}: {:?}", net.client(id).events);
        }
    }

    #[test]
    fn member_event_carries_safety_code() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let (pa, pb) = (net.client(a).identity.public(), net.client(b).identity.public());
        let code = super::super::safety_code(&pa, &pb);
        assert!(net.client(a).events.contains(&E2eEvent::Member {
            id: b,
            name: "b".into(),
            identity: pb,
            trust: Trust::NewUser,
            code: code.clone(),
        }));
        assert!(net.client(b).events.iter().any(|e| matches!(e,
            E2eEvent::Member { id, code: c, .. } if *id == a && *c == code)));
    }

    #[test]
    fn newcomer_cannot_open_frames_from_before_join() {
        let (mut net, ids) = room(&["a", "b"]);
        let a = ids[0];
        let old = net.frame(a);
        let c = net.join("c");
        net.deliver();
        // Before the switch a still seals with the pre-join key: c must not hear it.
        let during = net.frame(a);
        assert!(!net.opens(c, a, &during));
        net.tick(600);
        assert!(!net.opens(c, a, &old));
        assert!(net.speak(a).iter().all(|&(_, h)| h));
    }

    #[test]
    fn leaver_cannot_open_frames_after_rotation() {
        let (mut net, ids) = room(&["a", "b", "c"]);
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        assert!(net.all_hear());
        let start = net.relays.len();
        net.leave(c);
        net.deliver();
        net.tick(600);
        assert!(!net.relays[start..].iter().any(|r| r.0 == a && r.1 == c));
        let f = net.frame(a);
        assert!(!net.opens(c, a, &f));
        assert!(net.opens(b, a, &f));
    }

    #[test]
    fn leaving_within_window_gets_no_newer_key() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let c = net.join("c");
        net.deliver();
        net.tick(100);
        assert!(net.keys.iter().any(|k| k.0 == a && k.1 == c), "c got the join key");
        let start = net.relays.len();
        net.leave(c);
        net.deliver();
        net.tick(600);
        assert!(!net.relays[start..].iter().any(|r| r.0 == a && r.1 == c));
        let f = net.frame(a);
        assert!(!net.opens(c, a, &f));
        assert!(net.opens(b, a, &f));
    }

    #[test]
    fn rejoin_within_window_cannot_open_earlier_frames() {
        let (mut net, ids) = room(&["a", "b", "c"]);
        let (a, c) = (ids[0], ids[2]);
        let old = net.frame(a);
        net.leave(c);
        net.deliver();
        net.tick(100);
        let before = net.frame(a);
        let c2 = net.rejoin(c);
        net.deliver();
        assert!(!net.opens(c2, a, &old));
        assert!(!net.opens(c2, a, &before));
        net.tick(600);
        assert!(!net.opens(c2, a, &before));
        assert!(net.all_hear());
    }

    #[test]
    fn junk_then_new_device_gets_no_old_key() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        assert!(net.all_hear());
        let old = net.frame(a);
        net.inject(a, ServerMsg::Announce { from: b, blob: vec![1, 2, 3] });
        net.deliver();
        let start = net.keys.len();
        net.hijack(b);
        net.deliver();
        net.tick(600);
        net.tick(1100);
        let to_b2: Vec<u16> =
            net.keys[start..].iter().filter(|k| k.0 == a && k.1 == b).map(|k| k.2).collect();
        assert!(!to_b2.is_empty(), "the new device is paired");
        assert!(to_b2.iter().all(|&e| at_least(e, old.epoch.wrapping_add(1))), "{to_b2:?} vs {}", old.epoch);
        assert!(!net.opens(b, a, &old));
    }

    #[test]
    fn junk_then_delayed_new_identity_gets_no_used_key() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        assert!(net.all_hear());
        net.inject(a, ServerMsg::Announce { from: b, blob: vec![1, 2, 3] });
        net.deliver();
        net.tick(600);
        let f = net.frame(a);
        let start = net.keys.len();
        net.hijack(b);
        net.deliver();
        net.tick(600);
        net.tick(1100);
        let to_b2: Vec<u16> =
            net.keys[start..].iter().filter(|k| k.0 == a && k.1 == b).map(|k| k.2).collect();
        assert!(!to_b2.is_empty(), "the new device is paired");
        assert!(to_b2.iter().all(|&e| at_least(e, f.epoch.wrapping_add(1))), "{to_b2:?} vs {}", f.epoch);
        assert!(!net.opens(b, a, &f));
    }

    #[test]
    fn delayed_announce_gets_no_used_key() {
        let (mut net, ids) = room(&["a", "b"]);
        let a = ids[0];
        let c = net.join("c");
        let held = net.withhold(a, c);
        assert!(!held.is_empty());
        net.deliver();
        net.tick(600);
        let f = net.frame(a);
        let start = net.keys.len();
        for m in held {
            net.inject(a, m);
        }
        net.deliver();
        net.tick(1100);
        assert!(!net.opens(c, a, &f));
        let to_c: Vec<u16> =
            net.keys[start..].iter().filter(|k| k.0 == a && k.1 == c).map(|k| k.2).collect();
        assert!(!to_c.is_empty());
        assert!(to_c.iter().all(|&e| at_least(e, f.epoch.wrapping_add(1))), "{to_c:?} vs {}", f.epoch);
        net.tick(600);
        assert!(!net.opens(c, a, &f));
        assert!(net.all_hear());
    }

    #[test]
    fn long_session_newcomer_accepts_members() {
        const HOUR: u64 = 3_600_000;
        // Joining re-announces: the newcomer sees fresh announces despite stale cached ones.
        let (mut net, _) = room(&["a", "b"]);
        net.now += Duration::from_millis(2 * HOUR);
        net.now_ms += 2 * HOUR;
        let _c = net.join("c");
        net.deliver();
        net.tick(600);
        assert!(net.all_hear());
        // Ticks re-announce periodically: the server's cache never goes stale.
        let (mut net, ids) = room(&["a", "b"]);
        for _ in 0..5 {
            net.tick(HOUR / 2);
        }
        for (&id, name) in ids.iter().zip(["a", "b"]) {
            assert!(verify_announce(&net.announces[&id], name, ROOM, net.now_ms).is_ok());
        }
    }

    #[test]
    fn churn_does_not_starve_switch() {
        let (mut net, ids) = room(&["a", "b", "c"]);
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        let f0 = net.frame(a);
        assert!(net.opens(b, a, &f0));
        let e0 = f0.epoch;
        net.leave(c);
        net.deliver();
        let mut switched_at = None;
        for step in 1..=30u64 {
            if step % 4 == 0 {
                let msg = if step % 8 == 0 {
                    ServerMsg::MemberLeft { id: 90 }
                } else {
                    ServerMsg::MemberJoined(MemberInfo { id: 90, name: "x".into() })
                };
                net.inject(a, msg);
            }
            net.tick(100);
            let f = net.frame(a);
            if f.epoch != e0 && switched_at.is_none() {
                switched_at = Some(step * 100);
            }
            if switched_at.is_some() {
                assert!(!net.opens(c, a, &f), "leaver hears at {} ms", step * 100);
            }
            assert!(net.opens(b, a, &f), "gap at {} ms", step * 100);
        }
        assert!(switched_at.is_some_and(|t| t <= 1500), "switched at {switched_at:?}");
    }

    #[test]
    fn ghost_gets_no_key() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let ghost = 99;
        let b_blob = net.announces[&b].clone();
        net.inject(a, ServerMsg::MemberJoined(MemberInfo { id: ghost, name: "ghost".into() }));
        net.inject(a, ServerMsg::Announce { from: ghost, blob: b_blob });
        net.deliver();
        net.tick(600);
        net.tick(1100);
        assert!(net.client(a).events.contains(&E2eEvent::Rejected {
            id: ghost,
            reason: AnnounceError::NameMismatch
        }));
        assert!(!net.relays.iter().any(|(_, to, _)| *to == ghost));
        assert!(net.all_hear());
    }

    #[test]
    fn own_identity_is_never_paired() {
        let (mut net, ids) = room(&["a", "b"]);
        let a = ids[0];
        let twin = 98;
        let a_blob = net.announces[&a].clone();
        net.inject(a, ServerMsg::MemberJoined(MemberInfo { id: twin, name: "a".into() }));
        net.inject(a, ServerMsg::Announce { from: twin, blob: a_blob });
        net.deliver();
        net.tick(1100);
        assert!(net.client(a).events.contains(&E2eEvent::Rejected {
            id: twin,
            reason: AnnounceError::OwnIdentity
        }));
        assert!(!net.relays.iter().any(|(_, to, _)| *to == twin));
    }

    #[test]
    fn unknown_member_ignored() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let b_blob = net.announces[&b].clone();
        let (now, ms) = (net.now, net.now_ms);
        let e = &mut net.client(a).e2e;
        let out = e.announce(55, &b_blob, now, ms);
        assert!(out.send.is_empty() && out.events.is_empty());
        let out = e.relay(55, &[0, 1, 2], now);
        assert!(out.send.is_empty() && out.events.is_empty());
        let out = e.member_left(55, now);
        assert!(out.send.is_empty() && out.events.is_empty());
        assert!(!e.open(55, 0, 1, &[0; 40], now, &mut Vec::new()));
    }

    #[test]
    fn relay_from_wrong_member_ignored() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let c = 77;
        net.inject(a, ServerMsg::MemberJoined(MemberInfo { id: c, name: "c".into() }));
        net.deliver();
        let b_to_a: Vec<_> =
            net.relays.iter().filter(|r| r.0 == b && r.1 == a).map(|r| r.2.clone()).collect();
        assert!(!b_to_a.is_empty());
        let now = net.now;
        for blob in &b_to_a {
            let out = net.client(a).e2e.relay(c, blob, now);
            assert!(out.send.is_empty() && out.events.is_empty());
        }
        let f = net.frame(b);
        assert!(!net.opens(a, c, &f));
        // Also with a real channel to the impersonated member: b's blobs never install keys.
        let (mut net, ids) = room(&["a", "b", "c"]);
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        let b_to_a: Vec<_> =
            net.relays.iter().filter(|r| r.0 == b && r.1 == a).map(|r| r.2.clone()).collect();
        for blob in b_to_a {
            net.call(a, |e, now, _| e.relay(c, &blob, now));
        }
        net.deliver();
        net.tick(1100);
        assert!(net.all_hear());
        // a must not have filed b's key under c: a frame sealed as c with b's key fails.
        let b_key = net.client(b).e2e.current_key().clone();
        let now = net.now;
        let (mut ct, mut pt) = (Vec::new(), Vec::new());
        let e = AudioSealer::new(c, &b_key).seal(1000, b"opus", &mut ct).unwrap();
        assert!(!net.client(a).e2e.open(c, e, 1000, &ct, now, &mut pt));
        let e = AudioSealer::new(b, &b_key).seal(1000, b"opus", &mut ct).unwrap();
        assert!(net.client(a).e2e.open(b, e, 1000, &ct, now, &mut pt), "control: key is real");
    }

    fn same_device(old: &Identity) -> Identity {
        let mut bytes = Identity::generate().to_bytes();
        bytes[97..113].copy_from_slice(&old.public().device);
        Identity::from_bytes(&bytes).unwrap()
    }

    #[test]
    fn changed_key_gets_no_key_until_accepted() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        assert!(net.all_hear());
        net.leave(b);
        net.deliver();
        let old = net.gone.remove(&b).unwrap();
        let b2_id = Arc::new(same_device(&old.identity));
        let (dir, trust) = store();
        let start = net.relays.len();
        let b2 = net.join_with("b", b2_id.clone(), trust, dir, 0);
        net.deliver();
        net.tick(600);
        net.tick(1100);
        assert!(net.client(a).events.iter().any(|e| matches!(e,
            E2eEvent::Member { id, trust: Trust::KeyChanged, .. } if *id == b2)));
        assert!(!net.relays[start..].iter().any(|r| r.0 == a && r.1 == b2));
        assert!(net.speak(a).iter().all(|&(_, h)| !h));

        net.client(a).trust.lock().unwrap().accept("b", &b2_id.public());
        net.call(a, |e, _, _| e.trust_changed(b2));
        assert!(net.client(a).events.iter().any(|e| matches!(e,
            E2eEvent::Member { id, trust: Trust::Known, .. } if *id == b2)));
        net.deliver();
        net.tick(1100);
        net.tick(1100);
        assert!(net.all_hear());
    }

    #[test]
    fn rapid_churn_converges() {
        let (mut net, _) = room(&["a", "b"]);
        let c = net.join("c");
        net.deliver();
        net.tick(100);
        net.leave(c);
        net.deliver();
        net.tick(100);
        let _c2 = net.rejoin(c);
        net.deliver();
        net.tick(100);
        net.tick(600);
        assert!(net.all_hear());
    }

    #[test]
    fn no_audio_gap_during_rotation() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let _c = net.join("c");
        net.deliver();
        let mut epochs = Vec::new();
        for _ in 0..10 {
            let f = net.frame(a);
            epochs.push(f.epoch);
            assert!(net.opens(b, a, &f), "gap at epoch {}", f.epoch);
            net.tick(100);
        }
        assert_ne!(epochs.first(), epochs.last(), "the sealer switched keys");
    }

    #[test]
    fn rotation_coalesces_and_restarts_timer() {
        let (mut net, ids) = room(&["a"]);
        let a = ids[0];
        let start = net.client(a).e2e.next_epoch();
        let e0 = net.frame(a).epoch;
        net.call(a, |e, now, _| e.member_joined(&MemberInfo { id: 40, name: "x".into() }, now));
        net.tick(400);
        net.call(a, |e, now, _| e.member_left(40, now));
        net.tick(400);
        assert_eq!(net.frame(a).epoch, e0, "timer restarted by the second change");
        net.tick(200);
        assert_eq!(net.frame(a).epoch, e0.wrapping_add(2));
        assert_eq!(net.client(a).e2e.next_epoch(), start.wrapping_add(2));
    }

    #[test]
    fn epochs_wrap() {
        let (dir, trust) = store();
        let _d = dir;
        let now = Instant::now();
        let (mut e, out) =
            E2e::new(Arc::new(Identity::generate()), trust, 1, "a".into(), &[], u16::MAX, now, T0_MS);
        assert!(matches!(out.send[..], [ClientMsg::Announce { .. }]));
        assert_eq!(e.next_epoch(), 0);
        e.member_joined(&MemberInfo { id: 2, name: "b".into() }, now);
        e.tick(now + Duration::from_millis(600));
        assert_eq!(e.seal(1, b"x", &mut Vec::new()), Some(0));
        assert_eq!(e.next_epoch(), 1);
    }

    #[test]
    fn replayed_hello1_recovers() {
        let (mut net, ids) = room(&["a", "b", "c"]);
        let (x, y) = (ids[0], ids[1]);
        let (px, py) = (net.client(x).identity.public(), net.client(y).identity.public());
        let (i, r) = if px.sign < py.sign { (x, y) } else { (y, x) };
        let hello1 = net.relays.iter().find(|m| m.0 == i && m.1 == r).unwrap().2.clone();
        net.tick(1100);
        net.inject(r, ServerMsg::Relay { from: i, blob: hello1 });
        net.deliver();
        net.leave(ids[2]);
        net.deliver();
        net.tick(600);
        net.tick(1100);
        let f = net.frame(i);
        assert!(net.opens(r, i, &f), "responder hears initiator");
        let f = net.frame(r);
        assert!(net.opens(i, r, &f), "initiator hears responder");
    }

    #[test]
    fn hostile_input_never_panics() {
        let (mut net, ids) = room(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let junk: [&[u8]; 6] = [&[], &[0], &[1, 200], &[2, 9, 9, 9], &[0xff; 64], &[0; 2000]];
        for blob in junk {
            net.call(a, |e, now, ms| e.announce(b, blob, now, ms));
            net.call(a, |e, now, _| e.relay(b, blob, now));
            let now = net.now;
            assert!(!net.client(a).e2e.open(b, 0, 0, blob, now, &mut Vec::new()));
        }
        net.call(a, |e, now, _| e.member_left(a, now));
        net.call(a, |e, now, _| e.member_joined(&MemberInfo { id: a, name: "a".into() }, now));
        net.deliver();
        net.tick(1100);
    }
}
