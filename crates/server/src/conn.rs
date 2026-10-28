//! One client connection: login on the control stream, then audio relay.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use protocol::audio::{upstream_to_downstream, MAX_DATAGRAM};
use protocol::{
    encode_frame, ClientMsg, FrameDecoder, MemberId, MemberInfo, RejectReason, ServerMsg,
    PROTOCOL_VERSION,
};
use quinn::{Connection, RecvStream, SendStream, VarInt};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::auth::{hash_password, verify_password, LoginLimiter};
use crate::room::Room;

/// Application close code: the account logged in elsewhere.
pub const CLOSE_REPLACED: u32 = 1;
/// Application close code: the client broke the protocol.
pub const CLOSE_PROTOCOL: u32 = 2;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the peer to read a final message before closing.
const LINGER: Duration = Duration::from_secs(2);
const AUDIO_RATE_PER_S: u32 = 100;
const AUDIO_BURST: u32 = 20;

/// A room member as seen by other connections.
#[derive(Clone)]
pub(crate) struct Peer {
    conn: Connection,
    ctrl: mpsc::UnboundedSender<ServerMsg>,
}

pub(crate) struct Shared {
    pub(crate) room: Mutex<Room<Peer>>,
    pub(crate) limiter: Mutex<LoginLimiter>,
    /// Username → Argon2id hash.
    pub(crate) users: HashMap<String, String>,
    pub(crate) verify_slots: tokio::sync::Semaphore,
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
            conn.close(VarInt::from_u32(CLOSE_PROTOCOL), b"protocol violation");
            return Ok(());
        }
        _ => return Ok(()),
    };
    let ClientMsg::Hello { version, username, password } = hello;

    let ip = conn.remote_address().ip();
    if let Some(reason) = check_login(&shared, ip, version, &username, password).await {
        tracing::info!(%ip, user = %username, ?reason, "login rejected");
        send_and_close(send, &conn, ServerMsg::Rejected { reason }, 0).await;
        return Ok(());
    }

    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
    let id = join_room(&shared, &username, Peer { conn: conn.clone(), ctrl: ctrl_tx });
    tracing::info!(%ip, user = %username, id, "joined");
    let writer = tokio::spawn(write_ctrl(send, ctrl_rx, conn.clone()));

    let end = tokio::select! {
        e = control_loop(&mut recv, &mut dec) => e,
        e = datagram_loop(&conn, id, &shared) => e,
    };
    if let End::Violation = end {
        conn.close(VarInt::from_u32(CLOSE_PROTOCOL), b"protocol violation");
    }
    writer.abort();
    leave_room(&shared, id);
    tracing::info!(user = %username, id, "left");
    Ok(())
}

async fn check_login(
    shared: &Shared,
    ip: IpAddr,
    version: u16,
    username: &str,
    password: String,
) -> Option<RejectReason> {
    if version != PROTOCOL_VERSION {
        return Some(RejectReason::VersionMismatch { server: PROTOCOL_VERSION });
    }
    {
        // Count the attempt before verifying, so parallel guesses cannot all slip through.
        let mut limiter = shared.limiter.lock().unwrap();
        if limiter.is_blocked(ip, Instant::now()) {
            return Some(RejectReason::RateLimited);
        }
        limiter.record_failure(ip, Instant::now());
    }
    // Unknown users are checked against a dummy hash so timing does not reveal account names.
    let (hash, known) = match shared.users.get(username) {
        Some(h) => (h.clone(), true),
        None => (dummy_hash().to_owned(), false),
    };
    // Each Argon2 check takes ~19 MiB; bound how many run at once.
    let ok = match shared.verify_slots.acquire().await {
        Ok(_permit) => tokio::task::spawn_blocking(move || verify_password(&hash, &password))
            .await
            .unwrap_or(false),
        Err(_) => false,
    };
    if ok && known {
        shared.limiter.lock().unwrap().forgive(ip);
        None
    } else {
        Some(RejectReason::BadCredentials)
    }
}

fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| hash_password("dummy-password-for-timing"))
}

fn join_room(shared: &Shared, username: &str, peer: Peer) -> MemberId {
    let welcome_tx = peer.ctrl.clone();
    let mut room = shared.room.lock().unwrap();
    let outcome = room.join(username, peer);
    let others = room.peers(outcome.id);
    if let Some((old_id, old)) = &outcome.replaced {
        let _ = old.ctrl.send(ServerMsg::SessionReplaced);
        for p in &others {
            let _ = p.ctrl.send(ServerMsg::MemberLeft { id: *old_id });
        }
    }
    let me = MemberInfo { id: outcome.id, name: username.to_owned() };
    for p in &others {
        let _ = p.ctrl.send(ServerMsg::MemberJoined(me.clone()));
    }
    let _ = welcome_tx.send(ServerMsg::Welcome { you: outcome.id, members: outcome.members });
    outcome.id
}

fn leave_room(shared: &Shared, id: MemberId) {
    let mut room = shared.room.lock().unwrap();
    // `None` when this session was already replaced; its departure was announced then.
    if room.leave(id).is_some() {
        for p in room.peers(id) {
            let _ = p.ctrl.send(ServerMsg::MemberLeft { id });
        }
    }
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

async fn control_loop(recv: &mut RecvStream, dec: &mut FrameDecoder) -> End {
    // Protocol v1 has no client messages after Hello; anything else is a violation.
    match read_msg::<ClientMsg>(recv, dec).await {
        Ok(Some(_)) | Err(End::Violation) => End::Violation,
        Ok(None) | Err(End::Closed) => End::Closed,
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
        let peers = {
            let room = shared.room.lock().unwrap();
            // A replaced session lingers to read its notice but must not be heard.
            if !room.contains(id) {
                continue;
            }
            room.peers(id)
        };
        for p in peers {
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
}
