//! One client connection: login on the control stream, then room traffic and audio relay.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use protocol::audio::{upstream_to_downstream, MAX_DATAGRAM};
use protocol::{
    encode_frame, hello_version, normalize_username, Auth, ClientMsg, FrameDecoder, MemberId, MemberInfo,
    RejectReason, RoomChoice, RoomErrorReason, RoomId, RoomInfo, ServerMsg, CLOSE_BLOCKED, MAX_BLOB, MAX_CHAT_BLOB,
    MIN_PASSWORD_CHARS, PROTOCOL_VERSION, TOKEN_TTL_S,
};
use quinn::{Connection, RecvStream, SendStream, VarInt};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::auth::{hash_password, random_token, verify_password, LoginLimiter};
use crate::room::Registry;
use crate::store::{Account, RegisterError, Store, StoreError};
use crate::unix_now;

/// Application close code: the account logged in elsewhere.
pub const CLOSE_REPLACED: u32 = 1;
/// Application close code: the client broke the protocol.
pub const CLOSE_PROTOCOL: u32 = 2;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the store is checked for accounts and rooms the admin changed from the shell.
const STORE_POLL: Duration = Duration::from_secs(5);
/// How long to wait for the peer to read a final message before closing.
const LINGER: Duration = Duration::from_secs(2);
const AUDIO_RATE_PER_S: u32 = 100;
const AUDIO_BURST: u32 = 20;
/// Control messages per second and burst, per connection.
const CTRL_RATE_PER_S: u32 = 50;
const CTRL_BURST: u32 = 100;
/// Chat messages per second and burst, per connection, on top of the control limit.
pub const CHAT_RATE_PER_S: u32 = 5;
pub const CHAT_BURST: u32 = 20;
/// Replaced client runs remembered per account.
const RETIRED_PER_USER: usize = 8;

/// An online member as seen by other connections.
#[derive(Clone)]
pub(crate) struct Peer {
    conn: Connection,
    ctrl: mpsc::UnboundedSender<ServerMsg>,
    /// The client run this connection belongs to (`ClientMsg::Hello::session`).
    session: u64,
    /// Last key-exchange blob this member announced in its current room (opaque; never logged).
    announce: Option<Arc<[u8]>>,
}

pub(crate) struct Shared {
    pub(crate) registry: Mutex<Registry<Peer>>,
    /// Room changes lock it only while holding `registry`, so they cannot race; account
    /// calls lock it alone, off the async threads (`with_store`).
    pub(crate) store: Mutex<Store>,
    pub(crate) limiter: Mutex<LoginLimiter>,
    /// Checked for unknown users so timing does not reveal which accounts exist.
    pub(crate) dummy_hash: String,
    pub(crate) verify_slots: tokio::sync::Semaphore,
    /// Account key (lowercase name) → client runs that were replaced by a login elsewhere.
    /// They may not come back, or a client that missed its notice would kick out the newer one.
    pub(crate) retired: Mutex<HashMap<String, VecDeque<u64>>>,
}

/// Simple token bucket limiting audio datagrams per connection.
pub struct TokenBucket {
    rate_per_s: u32,
    burst: u32,
    tokens: f64,
    last: Option<Instant>,
}

impl TokenBucket {
    pub fn new(rate_per_s: u32, burst: u32) -> Self {
        Self { rate_per_s, burst, tokens: burst as f64, last: None }
    }

    pub fn try_take(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            let refill = now.saturating_duration_since(last).as_secs_f64() * self.rate_per_s as f64;
            self.tokens = (self.tokens + refill).min(self.burst as f64);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

enum End {
    Closed,
    Violation,
}

pub(crate) async fn handle(incoming: quinn::Incoming, shared: Arc<Shared>) {
    let Ok(conn) = incoming.await else { return };
    if let Err(e) = serve(conn, shared).await {
        tracing::debug!("connection ended: {e:#}");
    }
}

async fn serve(conn: Connection, shared: Arc<Shared>) -> anyhow::Result<()> {
    let (send, mut recv) = timeout(HELLO_TIMEOUT, conn.accept_bi()).await??;
    let mut dec = FrameDecoder::new();
    let hello = match timeout(HELLO_TIMEOUT, read_msg::<ClientMsg>(&mut recv, &mut dec)).await {
        Ok(Ok(Some(m))) => m,
        Ok(Err(End::Violation)) => {
            // A Hello of another protocol version may not decode; still tell it why.
            match dec.front_body().and_then(hello_version) {
                Some(v) if v != PROTOCOL_VERSION => {
                    let reason = RejectReason::VersionMismatch { server: PROTOCOL_VERSION };
                    send_and_close(send, &conn, ServerMsg::Rejected { reason }, 0).await;
                }
                _ => conn.close(VarInt::from_u32(CLOSE_PROTOCOL), b"protocol violation"),
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    let ClientMsg::Hello { version, username: claimed, auth, session, room } = hello else {
        conn.close(VarInt::from_u32(CLOSE_PROTOCOL), b"protocol violation");
        return Ok(());
    };

    let ip = conn.remote_address().ip();
    let kind = auth_kind(&auth);
    // Before the credentials are checked, so a returning replaced run does not spend its token
    // (the user's next start needs it).
    if is_retired(&shared, &claimed, session) {
        tracing::info!(%ip, user = ?logged_name(&claimed), "replaced client tried to come back");
        send_and_close(send, &conn, ServerMsg::SessionReplaced, CLOSE_REPLACED).await;
        return Ok(());
    }
    let username = match check_login(&shared, ip, version, &claimed, auth).await {
        Ok(name) => name,
        Err(reason) => {
            tracing::info!(%ip, user = ?logged_name(&claimed), kind, ?reason, "login rejected");
            send_and_close(send, &conn, ServerMsg::Rejected { reason }, 0).await;
            return Ok(());
        }
    };
    let Some(token) = issue_token(&shared, &username).await else {
        send_and_close(send, &conn, ServerMsg::Rejected { reason: RejectReason::RateLimited }, 0).await;
        return Ok(());
    };
    shared.limiter.lock().unwrap().forgive(ip);

    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
    let peer = Peer { conn: conn.clone(), ctrl: ctrl_tx, session, announce: None };
    let Some(id) = log_in(&shared, &username, peer, room, token) else {
        tracing::info!(%ip, user = %username, "replaced client tried to come back");
        send_and_close(send, &conn, ServerMsg::SessionReplaced, CLOSE_REPLACED).await;
        return Ok(());
    };
    tracing::info!(%ip, user = %username, kind, id, "joined");
    let writer = tokio::spawn(write_ctrl(send, ctrl_rx, conn.clone()));

    let end = tokio::select! {
        e = control_loop(&mut recv, &mut dec, id, &username, &shared) => e,
        e = datagram_loop(&conn, id, &shared) => e,
    };
    if let End::Violation = end {
        conn.close(VarInt::from_u32(CLOSE_PROTOCOL), b"protocol violation");
    }
    writer.abort();
    log_out(&shared, id);
    tracing::info!(user = %username, id, "left");
    Ok(())
}

/// A name from the network, as logged: escaped by `?` at the call site and cut short.
fn logged_name(name: &str) -> String {
    name.chars().take(64).collect()
}

fn auth_kind(auth: &Auth) -> &'static str {
    match auth {
        Auth::Password(_) => "password",
        Auth::Token(_) => "token",
        Auth::Register { .. } => "register",
    }
}

/// Returns the account's name as stored. Every kind of attempt counts against the IP until
/// the caller forgives it, so tokens and invites cannot be guessed faster than passwords.
async fn check_login(
    shared: &Arc<Shared>,
    ip: IpAddr,
    version: u16,
    username: &str,
    auth: Auth,
) -> Result<String, RejectReason> {
    if version != PROTOCOL_VERSION {
        return Err(RejectReason::VersionMismatch { server: PROTOCOL_VERSION });
    }
    {
        // Count the attempt before verifying, so parallel guesses cannot all slip through.
        let mut limiter = shared.limiter.lock().unwrap();
        if limiter.is_blocked(ip, Instant::now()) {
            return Err(RejectReason::RateLimited);
        }
        limiter.record_failure(ip, Instant::now());
    }
    match auth {
        Auth::Password(password) => password_login(shared, username, password).await,
        Auth::Token(token) => token_login(shared, username, token).await,
        Auth::Register { invite, password } => register(shared, username, invite, password).await,
    }
}

async fn password_login(
    shared: &Arc<Shared>,
    username: &str,
    password: String,
) -> Result<String, RejectReason> {
    let name = username.to_owned();
    let account = match with_store(shared, move |s| s.account(&name)).await {
        Some(Ok(account)) => account,
        other => return Err(store_failed("account lookup", other.map(|r| r.map(drop)))),
    };
    // Unknown users are checked against a dummy hash so timing does not reveal account names.
    let hash = account.as_ref().map_or_else(|| shared.dummy_hash.clone(), |a| a.hash.clone());
    if argon2(shared, move || verify_password(&hash, &password)).await != Some(true) {
        return Err(RejectReason::BadCredentials);
    }
    match account {
        None => Err(RejectReason::BadCredentials),
        Some(a) if a.blocked => Err(RejectReason::AccountBlocked),
        Some(a) => Ok(a.name),
    }
}

async fn token_login(
    shared: &Arc<Shared>,
    username: &str,
    token: [u8; 32],
) -> Result<String, RejectReason> {
    let claimed = username.to_lowercase();
    let account = with_store(shared, move |s| -> anyhow::Result<Option<Account>> {
        match s.redeem_token(&token, unix_now())? {
            Some(key) if key == claimed => s.account(&key),
            // Someone else's: a leaked token dies on first use, without a grace period.
            Some(_) => s.forget_token(&token).map(|()| None),
            None => Ok(None),
        }
    })
    .await;
    match account {
        Some(Ok(Some(a))) if !a.blocked => Ok(a.name),
        Some(Ok(_)) => Err(RejectReason::TokenInvalid),
        other => Err(store_failed("token redeem", other.map(|r| r.map(drop)))),
    }
}

async fn register(
    shared: &Arc<Shared>,
    username: &str,
    invite: [u8; 16],
    password: String,
) -> Result<String, RejectReason> {
    let Some(name) = normalize_username(username) else { return Err(RejectReason::NameInvalid) };
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(RejectReason::PasswordTooShort);
    }
    let Some(hash) = argon2(shared, move || hash_password(&password)).await else {
        return Err(store_failed("password hashing", None));
    };
    match with_store(shared, move |s| s.register(&invite, &name, &hash, unix_now())).await {
        Some(Ok(account)) => {
            tracing::info!(user = %account.name, "account registered");
            Ok(account.name)
        }
        Some(Err(RegisterError::InviteInvalid)) => Err(RejectReason::InviteInvalid),
        Some(Err(RegisterError::NameTaken)) => Err(RejectReason::NameTaken),
        Some(Err(RegisterError::Db(e))) => Err(store_failed("registration", Some(Err(e)))),
        None => Err(store_failed("registration", None)),
    }
}

/// A fresh session token for the next login, or `None` if the store failed.
async fn issue_token(shared: &Arc<Shared>, name: &str) -> Option<[u8; 32]> {
    let token = random_token();
    let name = name.to_owned();
    match with_store(shared, move |s| s.issue_token(&name, &token, unix_now(), TOKEN_TTL_S)).await {
        Some(Ok(())) => Some(token),
        other => {
            store_failed("token issue", other);
            None
        }
    }
}

/// A server-side failure: the client is told to retry later rather than that it was wrong.
/// `outcome` is what the store call returned (`None`: it panicked); its error is logged with
/// its whole chain of causes.
fn store_failed(what: &str, outcome: Option<anyhow::Result<()>>) -> RejectReason {
    match outcome {
        Some(Err(e)) => tracing::warn!("account store failed during {what}: {e:#}"),
        None => tracing::warn!("account store failed during {what}: the store call panicked"),
        Some(Ok(())) => tracing::warn!("account store failed during {what}"),
    }
    RejectReason::RateLimited
}

/// Runs `f` on the store off the async threads; `None` if it panicked.
async fn with_store<R: Send + 'static>(
    shared: &Arc<Shared>,
    f: impl FnOnce(&Store) -> R + Send + 'static,
) -> Option<R> {
    let shared = shared.clone();
    tokio::task::spawn_blocking(move || f(&shared.store.lock().unwrap())).await.ok()
}

/// Each Argon2 run takes ~19 MiB; bound how many run at once.
async fn argon2<R: Send + 'static>(
    shared: &Shared,
    f: impl FnOnce() -> R + Send + 'static,
) -> Option<R> {
    let _permit = shared.verify_slots.acquire().await.ok()?;
    tokio::task::spawn_blocking(f).await.ok()
}

/// Picks up what the admin changed from the shell: blocked accounts are disconnected and
/// rooms created or deleted there reach the running server and its clients.
pub(crate) async fn watch_store(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(STORE_POLL);
    loop {
        tick.tick().await;
        kick_blocked(&shared).await;
        sync_rooms(&shared).await;
    }
}

/// Closes the connections of accounts blocked since they logged in.
async fn kick_blocked(shared: &Arc<Shared>) {
    let blocked = match with_store(shared, |s| s.blocked_accounts()).await {
        Some(Ok(names)) => names.iter().map(|n| n.to_lowercase()).collect::<HashSet<_>>(),
        other => {
            store_failed("blocked accounts check", other.map(|r| r.map(drop)));
            return;
        }
    };
    if blocked.is_empty() {
        return;
    }
    let reg = shared.registry.lock().unwrap();
    let (_, members) = reg.snapshot();
    for m in members.iter().filter(|m| blocked.contains(&m.name.to_lowercase())) {
        if let Some(p) = reg.get(m.id) {
            tracing::info!(user = %m.name, id = m.id, "blocked account disconnected");
            p.conn.close(VarInt::from_u32(CLOSE_BLOCKED), b"blocked");
        }
    }
}

/// Mirrors the stored room list into the registry. The list is read while holding the
/// registry lock, like the room handlers do, so a room being created by a client is not
/// mistaken for a deleted one.
async fn sync_rooms(shared: &Arc<Shared>) {
    let shared = shared.clone();
    let done = tokio::task::spawn_blocking(move || {
        let mut reg = shared.registry.lock().unwrap();
        let stored = shared.store.lock().unwrap().rooms();
        match stored {
            Ok(stored) => apply_room_changes(&mut reg, &stored),
            Err(e) => {
                store_failed("room refresh", Some(Err(e)));
            }
        }
    })
    .await;
    if done.is_err() {
        tracing::warn!("room refresh panicked");
    }
}

fn apply_room_changes(reg: &mut Registry<Peer>, stored: &[RoomInfo]) {
    for info in stored {
        if !reg.has_room(info.id) {
            reg.add_room(info.clone());
            broadcast(reg, ServerMsg::RoomCreated(info.clone()));
        }
    }
    for room in reg.room_ids().into_iter().filter(|id| !stored.iter().any(|r| r.id == *id)) {
        for id in reg.members_in(room) {
            if reg.move_to(id, None) == Ok(true) {
                // It was meant for the old room's members.
                if let Some(me) = reg.get_mut(id) {
                    me.announce = None;
                }
                broadcast(reg, ServerMsg::MemberMoved { id, room: None });
            }
        }
        reg.remove_room(room);
        broadcast(reg, ServerMsg::RoomDeleted { room });
    }
}

fn broadcast(reg: &Registry<Peer>, msg: ServerMsg) {
    for p in reg.everyone() {
        let _ = p.ctrl.send(msg.clone());
    }
}

/// Whether client run `session` of `username`'s account was replaced by a login elsewhere.
fn is_retired(shared: &Shared, username: &str, session: u64) -> bool {
    shared.retired.lock().unwrap().get(&username.to_lowercase()).is_some_and(|r| r.contains(&session))
}

/// `None` when this client run was already replaced by a login elsewhere.
fn log_in(
    shared: &Shared,
    username: &str,
    peer: Peer,
    choice: RoomChoice,
    token: [u8; 32],
) -> Option<MemberId> {
    let welcome_tx = peer.ctrl.clone();
    let session = peer.session;
    let mut reg = shared.registry.lock().unwrap();
    let mut retired = shared.retired.lock().unwrap();
    let account_key = username.to_lowercase();
    // Checked again under the registry lock: the run may have been replaced since.
    if retired.get(&account_key).is_some_and(|r| r.contains(&session)) {
        return None;
    }
    let outcome = reg.login(username, peer, choice);
    let others = reg.others(outcome.id);
    if let Some((old_id, old)) = &outcome.replaced {
        // The same run reconnecting over its own stale connection is not a replacement.
        if old.session != session {
            let r = retired.entry(account_key).or_default();
            if r.len() == RETIRED_PER_USER {
                r.pop_front();
            }
            r.push_back(old.session);
        }
        let _ = old.ctrl.send(ServerMsg::SessionReplaced);
        for p in &others {
            let _ = p.ctrl.send(ServerMsg::MemberLeft { id: *old_id });
        }
    }
    let me = MemberInfo { id: outcome.id, name: username.to_owned(), room: outcome.room };
    for p in &others {
        let _ = p.ctrl.send(ServerMsg::MemberJoined(me.clone()));
    }
    let (rooms, members) = reg.snapshot();
    let _ = welcome_tx.send(ServerMsg::Welcome { you: outcome.id, room: outcome.room, rooms, members, token });
    // Same lock as Welcome, so these always follow it and precede any later Announce.
    replay_announces(&reg, outcome.id, &welcome_tx);
    Some(outcome.id)
}

fn log_out(shared: &Shared, id: MemberId) {
    let mut reg = shared.registry.lock().unwrap();
    // `None` when this session was already replaced; its departure was announced then.
    if reg.leave(id).is_some() {
        for p in reg.others(id) {
            let _ = p.ctrl.send(ServerMsg::MemberLeft { id });
        }
    }
}

/// Sends `to` the stored announces of the others in `id`'s room.
fn replay_announces(reg: &Registry<Peer>, id: MemberId, to: &mpsc::UnboundedSender<ServerMsg>) {
    for (from, p) in reg.room_peers(id) {
        if let Some(blob) = &p.announce {
            let _ = to.send(ServerMsg::Announce { from, blob: blob.to_vec() });
        }
    }
}

fn send_to(reg: &Registry<Peer>, id: MemberId, msg: ServerMsg) {
    if let Some(p) = reg.get(id) {
        let _ = p.ctrl.send(msg);
    }
}

/// To everyone online, `id` included.
fn send_to_all(reg: &Registry<Peer>, id: MemberId, msg: ServerMsg) {
    for p in reg.others(id) {
        let _ = p.ctrl.send(msg.clone());
    }
    send_to(reg, id, msg);
}

async fn write_ctrl(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<ServerMsg>, conn: Connection) {
    while let Some(msg) = rx.recv().await {
        if matches!(msg, ServerMsg::SessionReplaced) {
            send_and_close(send, &conn, msg, CLOSE_REPLACED).await;
            return;
        }
        if send.write_all(&encode_frame(&msg)).await.is_err() {
            return;
        }
    }
}

/// Sends a final message, gives the peer a moment to read it, then closes.
async fn send_and_close(mut send: SendStream, conn: &Connection, msg: ServerMsg, code: u32) {
    if send.write_all(&encode_frame(&msg)).await.is_ok() && send.finish().is_ok() {
        let _ = timeout(LINGER, send.stopped()).await;
    }
    conn.close(VarInt::from_u32(code), b"");
}

async fn control_loop(
    recv: &mut RecvStream,
    dec: &mut FrameDecoder,
    id: MemberId,
    username: &str,
    shared: &Shared,
) -> End {
    let mut bucket = TokenBucket::new(CTRL_RATE_PER_S, CTRL_BURST);
    let mut chat_bucket = TokenBucket::new(CHAT_RATE_PER_S, CHAT_BURST);
    loop {
        let msg = match read_msg::<ClientMsg>(recv, dec).await {
            Ok(Some(m)) => m,
            Err(End::Violation) => return End::Violation,
            Ok(None) | Err(End::Closed) => return End::Closed,
        };
        match msg {
            ClientMsg::Announce { blob } | ClientMsg::Relay { blob, .. } if blob.len() > MAX_BLOB => {
                return End::Violation;
            }
            ClientMsg::Chat { blob } if blob.len() > MAX_CHAT_BLOB => return End::Violation,
            ClientMsg::Hello { .. } => return End::Violation,
            _ if !bucket.try_take(Instant::now()) => {}
            ClientMsg::Chat { .. } if !chat_bucket.try_take(Instant::now()) => {}
            ClientMsg::Announce { blob } => {
                let mut reg = shared.registry.lock().unwrap();
                // A replaced session lingers to read its notice but must not be heard.
                let Some(me) = reg.get_mut(id) else { continue };
                me.announce = Some(Arc::from(blob.as_slice()));
                for (_, p) in reg.room_peers(id) {
                    let _ = p.ctrl.send(ServerMsg::Announce { from: id, blob: blob.clone() });
                }
            }
            ClientMsg::Relay { to, blob } => {
                let reg = shared.registry.lock().unwrap();
                let room = reg.room_of(id);
                if to != id && room.is_some() && reg.room_of(to) == room {
                    send_to(&reg, to, ServerMsg::Relay { from: id, blob });
                }
            }
            // Opaque and never logged.
            ClientMsg::Chat { blob } => {
                let reg = shared.registry.lock().unwrap();
                for (_, p) in reg.room_peers(id) {
                    let _ = p.ctrl.send(ServerMsg::Chat { from: id, blob: blob.clone() });
                }
            }
            ClientMsg::JoinRoom { room } => move_member(shared, id, Some(room)),
            ClientMsg::LeaveRoom => move_member(shared, id, None),
            ClientMsg::CreateRoom { name } => create_room(shared, id, username, &name),
            ClientMsg::DeleteRoom { room } => delete_room(shared, id, username, room),
        }
    }
}

fn move_member(shared: &Shared, id: MemberId, room: Option<RoomId>) {
    let mut reg = shared.registry.lock().unwrap();
    match reg.move_to(id, room) {
        Err(reason) => send_to(&reg, id, ServerMsg::RoomError { reason }),
        Ok(false) => {}
        Ok(true) => {
            // It was meant for the old room's members.
            if let Some(me) = reg.get_mut(id) {
                me.announce = None;
            }
            send_to_all(&reg, id, ServerMsg::MemberMoved { id, room });
            if let Some(me) = reg.get(id) {
                replay_announces(&reg, id, &me.ctrl);
            }
        }
    }
}

fn create_room(shared: &Shared, id: MemberId, username: &str, name: &str) {
    let mut reg = shared.registry.lock().unwrap();
    // A replaced session lingers to read its notice but may not act.
    if !reg.contains(id) {
        return;
    }
    let name = match reg.check_new_room(name) {
        Ok(name) => name,
        Err(reason) => return send_to(&reg, id, ServerMsg::RoomError { reason }),
    };
    let created = shared.store.lock().unwrap().create_room(&name, username);
    match created {
        Ok(info) => {
            reg.add_room(info.clone());
            send_to_all(&reg, id, ServerMsg::RoomCreated(info));
        }
        Err(StoreError::NameTaken) => {
            send_to(&reg, id, ServerMsg::RoomError { reason: RoomErrorReason::NameTaken });
        }
        Err(StoreError::Db(_)) => {
            tracing::warn!("room store failed to create a room");
            send_to(&reg, id, ServerMsg::RoomError { reason: RoomErrorReason::NotFound });
        }
    }
}

fn delete_room(shared: &Shared, id: MemberId, username: &str, room: RoomId) {
    let mut reg = shared.registry.lock().unwrap();
    if !reg.contains(id) {
        return;
    }
    // Read on every delete, so an admin made or unmade from the shell counts at once.
    let admin = matches!(shared.store.lock().unwrap().account(username), Ok(Some(a)) if a.admin);
    if let Err(reason) = reg.check_delete(room, username, admin) {
        return send_to(&reg, id, ServerMsg::RoomError { reason });
    }
    let deleted = shared.store.lock().unwrap().delete_room(room);
    match deleted {
        Ok(_) => {
            reg.remove_room(room);
            send_to_all(&reg, id, ServerMsg::RoomDeleted { room });
        }
        Err(_) => {
            tracing::warn!("room store failed to delete a room");
            send_to(&reg, id, ServerMsg::RoomError { reason: RoomErrorReason::NotFound });
        }
    }
}

async fn datagram_loop(conn: &Connection, id: MemberId, shared: &Shared) -> End {
    let mut bucket = TokenBucket::new(AUDIO_RATE_PER_S, AUDIO_BURST);
    loop {
        let Ok(d) = conn.read_datagram().await else { return End::Closed };
        if d.len() > MAX_DATAGRAM {
            return End::Violation;
        }
        if !bucket.try_take(Instant::now()) {
            continue;
        }
        let Ok(down) = upstream_to_downstream(id, &d) else { continue };
        let down = Bytes::from(down);
        // Empty for a session in no room, or a replaced one lingering to read its notice.
        let peers = shared.registry.lock().unwrap().room_peers(id);
        for (_, p) in peers {
            let _ = p.conn.send_datagram(down.clone());
        }
    }
}

/// Reads one framed message; `Ok(None)` when the stream ends cleanly.
async fn read_msg<T: DeserializeOwned>(
    recv: &mut RecvStream,
    dec: &mut FrameDecoder,
) -> Result<Option<T>, End> {
    let mut buf = [0u8; 4096];
    loop {
        match dec.next::<T>() {
            Ok(Some(m)) => return Ok(Some(m)),
            Ok(None) => {}
            Err(_) => return Err(End::Violation),
        }
        match recv.read(&mut buf).await {
            Ok(Some(n)) => dec.push(&buf[..n]),
            Ok(None) => return Ok(None),
            Err(_) => return Err(End::Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_refills() {
        let mut b = TokenBucket::new(100, 20);
        let t0 = Instant::now();
        assert_eq!((0..21).filter(|_| b.try_take(t0)).count(), 20);
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!((0..20).filter(|_| b.try_take(t1)).count(), 10);
    }

    #[test]
    fn claimed_names_are_logged_short_and_escaped() {
        let long = "ż".repeat(100);
        assert_eq!(format!("{:?}", logged_name(&long)), format!("{:?}", "ż".repeat(64)));
        // A newline cannot fake a second log line.
        assert_eq!(format!("{:?}", logged_name("ala\n[fake] joined")), r#""ala\n[fake] joined""#);
    }
}
